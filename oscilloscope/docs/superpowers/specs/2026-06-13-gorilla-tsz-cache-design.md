# Gorilla TSZ 缓存格式设计

## 背景

当前数据缓存使用 Parquet + zstd 格式，存在以下瓶颈：

1. **加载路径长：** Parquet → zstd 解压 → Parquet 解析 → Arrow Float64Array → Vec<[f64;2]> → f32 → GPU
2. **依赖重：** polars (44), arrow (54), parquet (54) 三个大依赖
3. **文件体积大：** 10 亿行 × 9 列 f64 原始 = 720 GB，Parquet+zstd 压缩后 ~120-180 GB
4. **随机访问开销：** Parquet row group 粒度粗，读取单个 chunk 仍需解析 page header

## 目标

- 用 Gorilla 时间序列压缩算法替换 Parquet
- 每个通道独立存储为 `.tsz` 文件（timestamp+value 配对流）
- 直接使用 `tsz` crate（[github.com/jeromefroe/tsz-rs](https://github.com/jeromefroe/tsz-rs)），不自定义编解码器
- 去掉 polars/arrow/parquet 依赖
- 保持现有 chunked 架构（100K 行/chunk + LRU 缓存 + 二进制索引）

## 压缩效果估算（10 亿行 × 8 通道）

| 指标 | 当前 Parquet | Gorilla TSZ |
|------|-------------|-------------|
| 时间列压缩比 | ~3:1 | ~100:1 (delta-of-delta) |
| 电压列压缩比 | ~3:1 | ~10:1-30:1 (XOR) |
| 总磁盘占用 | ~120-180 GB | ~15-30 GB |
| 加载路径 | 5 步 | 3 步 |

## 文件格式

### 缓存目录结构

```
.oscv/<file_stem>/
    metadata.json          ← 元数据（复用现有格式，扩展字段）
    index.bin              ← chunk 索引（复用现有二进制格式）
    chunks/
        chunk_000000/
            ch0.tsz        ← 通道 0 的 Gorilla 压缩流
            ch1.tsz        ← 通道 1
            ...
            ch7.tsz        ← 通道 7
        chunk_000001/
            ch0.tsz
            ...
```

### metadata.json

```json
{
  "version": 3,
  "md5": "...",
  "total_rows": 1000000000,
  "n_cols": 9,
  "col_names": ["time", "ch0", "ch1", "ch2", "ch3", "ch4", "ch5", "ch6", "ch7"],
  "time_range": [0.0, 10.0],
  "rows_per_chunk": 100000,
  "chunked": true,
  "format": "tsz"
}
```

### index.bin（不变）

复用现有二进制索引格式：
- Magic: `OSCVIDX\0`
- Version: 3（区分 Parquet 版本 2）
- Header: n_cols, n_chunks, rows_per_chunk, total_rows, x_min, x_max
- Per-chunk: t_min, t_max, row_count, reserved, file_size

### .tsz 文件内容

每个 `.tsz` 文件是 `tsz` crate 的 `StdEncoder` 输出：
- 100,000 个 `DataPoint { timestamp: u64, value: f64 }`
- 时间戳通过 `f64::to_bits()` 转为 u64（无损 bitcast）
- 电压值直接使用 f64
- Gorilla 算法：时间列 delta-of-delta + 电压列 XOR 编码

## 数据流

### CSV → TSZ 转换

```
CSV 文件
  → csv crate 流式读取
  → 每 100K 行一组
  → 每个通道独立 StdEncoder::encode()
  → StdEncoder::close() 得到压缩 bytes
  → 写入 chunk_NNNNNN/chM.tsz
  → 写入 index.bin + metadata.json
```

### 查询加载

```
请求时间范围 [t_min, t_max]
  → index.bin 二进制定位需要的 chunk
  → LRU 缓存查找
  → 未命中: 读取 chunk_NNNNNN/chM.tsz
    → zstd 解压（tsz crate 内部无压缩，直接读取 bitstream）
    → StdDecoder 迭代得到 DataPoint 序列
    → 转为 Vec<[f64; 2]> (time, value)
    → 存入 LRU 缓存
  → M4 降采样 → StripCache → 渲染
```

### GPU 密度渲染

```
StripCache.points (Vec<[f64; 2]>)
  → 转为 f32 数组
  → queue.write_buffer() 上传 GPU
  → compute shader 计算密度图
```

## 实现计划

### 第 1 步：添加 tsz 依赖，实现 TSZ 编解码模块

**文件：** `src/data/tsz_codec.rs`

- 封装 `tsz` crate 的 StdEncoder/StdDecoder
- 提供 `encode_channel(timestamps: &[f64], values: &[f64]) -> Vec<u8>`
- 提供 `decode_channel(data: &[u8]) -> Vec<(f64, f64)>`
- f64 时间戳 ↔ u64 bitcast 转换
- 单元测试

### 第 2 步：实现 TSZ 缓存写入

**文件：** `src/data/cache.rs`（修改）

- 新增 `convert_csv_to_tsz()` 函数
- 替换 `convert_csv_to_parquet()`
- 每个通道独立编码为 `.tsz` 文件
- 复用现有 index.bin 写入逻辑
- metadata.json 中 format 字段设为 "tsz"

### 第 3 步：实现 TSZ ChunkStore

**文件：** `src/data/chunk_store.rs`（修改）

- 新增 `TszChunkStore` 结构
- 加载 `.tsz` 文件并解码为 `Vec<[f64; 2]>`
- 复用现有 LRU 缓存和 chunk 索引逻辑
- `get_channel_points()` 和 `get_raw_points()` 适配新数据源

### 第 4 步：适配 WaveformData 枚举

**文件：** `src/data.rs`（修改）

- `WaveformData` 枚举新增 `Tsz(TszData)` 变体
- `TszData` 包含 `TszChunkStore` 和元数据
- 实现 `get_channel_points()`, `get_raw_points()`, `compute_channel_stats()`
- `load_csv()` 根据 metadata.format 选择加载路径

### 第 5 步：移除 Parquet 依赖

**文件：** `Cargo.toml`, `src/data/cache.rs`, `src/data/chunk_store.rs`, `src/data.rs`

- 移除 polars, arrow, parquet 依赖
- 移除 ParquetData, ParquetBackend 相关代码
- 更新 Cargo.toml description

### 第 6 步：迁移兼容

- 自动检测旧 Parquet 缓存并重新转换
- 或保留 Parquet 读取能力作为过渡（可选）

## 测试策略

- 单元测试：tsz_codec 编解码往返一致性
- 集成测试：CSV → TSZ → 查询结果与原始 CSV 一致
- 性能测试：10 亿行文件的转换时间、查询延迟、磁盘占用
