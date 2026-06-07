# 用 Rust 写一个高性能示波器波形查看器 — 支持眼图、FFT、自动测量

> 项目地址：https://github.com/weiwangfds/vamame
>
> 支持 Windows / macOS / Linux 三平台，开箱即用

## 前言

做高速信号完整性分析的同学应该都有这样的痛点：示波器导出的 CSV 数据动辄几十 MB、上百万行，用 Excel 打不开，用 Python 画图又慢又卡。商业软件如 Infiniium、ScopeTrader 价格不菲，开源工具又大多功能单一。

所以我用 Rust 写了一个轻量级的示波器波形查看分析工具，核心特性：

- **百万行数据秒开** — 基于 Polars + Parquet 缓存引擎
- **流畅缩放平移** — M4 降采样算法，恒定 4000 点渲染
- **一键眼图分析** — 自动识别 UI 周期，8 种荧光屏配色
- **FFT 频谱分析** — 支持多种窗函数，dB/线性切换
- **全平台可用** — Windows / macOS / Linux 均有预编译二进制

## 功能速览

### 1. 多通道波形显示

直接打开 CSV 文件，每列自动识别为一个通道，独立分栏显示。所有分栏共享时间轴，缩放/平移联动。

![波形显示](docs/images/waveform.png)

支持的交互操作：

- 滚轮缩放，拖拽平移
- 通道拖拽合并/拆分
- 双击重命名通道
- 每个通道可独立设置延迟
- 撤销缩放（最多 50 级历史）
- 跳转到指定时间点（支持科学计数法输入）

**性能方面**：即使数据有 10 万行，打开后缩放/平移依然流畅。秘诀是采用了 M4 降采样算法 — 只渲染可见范围内的 4000 个代表点（每段的 min/max/first/last），不管原始数据有多大。

### 2. 自动测量

工具栏一键开启测量面板，自动计算以下参数：

| 电压测量 | 时间测量 |
|---------|---------|
| Vpp 峰峰值 | Freq 频率 |
| Vmax / Vmin | Period 周期 |
| Vmean 平均值 | Rise 上升时间 |
| Vrms 有效值 | Fall 下降时间 |
|  | Duty 占空比 |
|  | +Width / -Width 脉宽 |

配合光标使用时，可以开启 **测量门控** — 只在光标 A-B 范围内计算测量值，精确定位感兴趣的区间。

### 3. 眼图分析（Eye Diagram）

这是这个工具的核心亮点之一。点击工具栏的 **Eye** 按钮即可打开眼图窗口。

**关键特性：**

- **自动 UI 检测** — 不需要手动输入符号周期，点击 "Auto UI" 自动识别
  - 基于 Schmitt 触发边沿检测（带滞回，抗噪声）
  - 间隙聚类分离 1×UI / 2×UI / 3×UI 等谐波
  - 去掉首尾 10% 离群值后取平均，精度很高
- **外部时钟通道** — 支持指定独立的时钟信号作为触发源
- **连续轨迹渲染** — 相邻采样点之间用 DDA 线段光栅化连线，不是散点图
- **对数压缩归一化** — `ln(1+v) / ln(1+max)` 压缩动态范围，轨迹和眼图内部都清晰可见
- **8 种颜色模式** — Phosphor（CRT 绿色荧光屏风格）、Rainbow、Temperature、Viridis 等

支持的参数调节：
- UI 周期（手动输入或自动检测）
- 显示 UI 数量（2~8 个）
- 饱和度（0.5~4.0）
- 时钟极性（上升沿 / 下降沿 / 双边沿）

### 4. FFT 频谱分析

点击 **FFT** 按钮打开频谱窗口，支持：

- 通道选择
- 三种窗函数：Rectangle / Hanning / Blackman-Harris
- dB 或线性幅度刻度
- 最多 131,072 采样点，自动补零到 2 的幂次
- 独立窗口，支持缩放和拖拽

### 5. XY 模式（Lissajous 图）

选择任意两个通道做参数曲线显示，适用于：
- 相位关系分析
- 信号对齐调试
- I/Q 信号星座图观察

### 6. 数学通道

支持 7 种数学运算创建虚拟通道：

- 加减乘：`CH1 + CH2`、`CH1 - CH2`、`CH1 × CH2`
- 取反：`-CH1`
- 绝对值：`|CH1|`
- 微分：`d(CH1)/dt`
- 积分：`∫CH1 dt`

数学通道创建后和普通通道一样，可以叠加显示、测量、导出。

### 7. 数据导入导出

- **CSV 导入**：后台线程加载，实时显示进度条（行数、MB、百分比）
- **Parquet 缓存**：首次加载自动转换，再次打开秒级加载
- **导出 CSV**：保存可见范围的数据
- **导出 PNG**：截图当前波形

## 数据格式要求

工具接受的 CSV 格式非常简单 — 纯数值、逗号分隔、无表头：

```csv
-1.32094967E-06, -3.608419E-02, -7.65844E-03, 2.50038E-03, -1.000E-03, 5.663E-03
-1.32093405E-06, 9.484693E-02, -6.89313E-03, 2.88304E-03, -1.051E-03, 2.041E-03
```

| 列 | 含义 |
|----|------|
| 第 0 列 | 时间轴（秒），单调递增 |
| 第 1 列起 | 各通道电压数据（伏特） |

数值支持标准浮点数和科学计数法（`E` 或 `e`），自动去除空白字符。

**兼容性**：大多数示波器（Keysight、Tektronix、RIGOL 等）导出的 CSV 格式都直接兼容。

## 快速上手

### 方式一：下载即用

前往 [GitHub Releases](https://github.com/weiwangfds/vamame/releases) 下载对应平台二进制：

| 平台 | 文件 |
|------|------|
| Windows | `oscilloscope-v*-windows-x86_64.exe` |
| macOS Intel | `oscilloscope-v*-macos-x86_64` |
| macOS Apple Silicon | `oscilloscope-v*-macos-arm64` |
| Linux x86_64 | `oscilloscope-v*-linux-x86_64` |

Windows 双击 exe 即可运行，macOS/Linux 添加执行权限后运行：

```bash
chmod +x oscilloscope-v*-macos-arm64
./oscilloscope-v*-macos-arm64
```

### 方式二：命令行启动并加载文件

```bash
# 直接指定 CSV 文件路径
oscilloscope /path/to/data.csv
```

### 方式三：从源码构建

```bash
git clone https://github.com/weiwangfds/vamame.git
cd vamame
cargo build --manifest-path oscilloscope/Cargo.toml --release
./oscilloscope/target/release/oscilloscope
```

## 技术架构

如果你对实现细节感兴趣，这里是技术选型：

| 组件 | 技术 | 说明 |
|------|------|------|
| GUI | egui / eframe | 即时模式 GUI，启动快、占用小 |
| 绘图 | egui_plot | 原生交互式图表，支持缩放/拖拽 |
| 数据引擎 | Polars (LazyFrame) | 列式计算引擎，支持谓词下推 |
| 存储格式 | Parquet | 列式存储，行组统计信息加速范围查询 |
| 降采样 | M4 算法 | 每段保留 min/max/first/last 四个代表点 |
| FFT | rustfft | 纯 Rust FFT 实现 |
| 眼图光栅化 | DDA + 双线性 splat | 连续轨迹 + 亚像素抗锯齿 |

**为什么快？**

1. **Parquet 谓词下推** — 缩放时只读取可见范围的行组，不会加载整个文件
2. **M4 降采样** — 无论数据多大，渲染始终只有 ~4000 点
3. **增量缓存** — 每个通道的降采样结果独立缓存，只在可见范围变化时重新计算
4. **后台加载** — CSV 解析在独立线程，UI 不卡顿

## 开发计划

- [ ] 波形持久化（保存/恢复视图状态）
- [ ] 协议解码（UART、SPI、I2C）
- [ ] 多文件叠加对比
- [ ] 眼图 mask 测试
- [ ] 抖动分析（TJ/RJ/DJ 分离）

## 最后

如果你也在做信号完整性、高速数字设计或者嵌入式调试，不妨试试这个工具。有任何问题或建议，欢迎在 [GitHub Issues](https://github.com/weiwangfds/vamame/issues) 反馈。

如果觉得有用，给个 Star 就是最大的鼓励！

---

**项目地址**：https://github.com/weiwangfds/vamame

**License**：MIT
