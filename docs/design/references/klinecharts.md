# KLineChart 集成最佳实践

> 项目使用 **v9.8.12**（推荐稳定版本）
> v10.0.0-beta2  仍在 beta，待正式发布后评估升级

## 一句话定位

KLineChart 是一个轻量级（40KB gzipped）、零依赖、高度可定制的 HTML5 Canvas 金融图表库，专为 K 线和衍生市场场景设计。v9 已足够稳定；v10 Beta 修复了 setDataLoader 的 y 轴自适应问题，但未正式发布。**当前推荐：先用 v9 完成核心功能，预留 v10 升级路径。**

---

## Quick Start（粘贴可用）

### 最小化 React + TypeScript 示例

```typescript
// KLineChartContainer.tsx
import React, { useEffect, useRef } from 'react';
import { init, dispose } from 'klinecharts';
import type { KLineData, Chart } from 'klinecharts';

interface KLineChartProps {
  symbol: string; // e.g. '600000'
  period: string; // e.g. '1d'
}

export const KLineChartContainer: React.FC<KLineChartProps> = ({ symbol, period }) => {
  const containerRef = useRef<HTMLDivElement>(null);
  const chartRef = useRef<Chart | null>(null);

  useEffect(() => {
    if (!containerRef.current) return;

    // 1. 初始化图表
    const chart = init(containerRef.current, {
      locale: 'zh-CN', // 中文
    });

    if (!chart) return;

    chartRef.current = chart;

    // 2. 配置 A 股红涨绿跌样式
    chart.setStyles({
      candle: {
        bar: {
          upColor: '#FF0000',      // 红（上涨）
          downColor: '#00B050',    // 绿（下跌）
          noChangeColor: '#888888',
          upBorderColor: '#FF0000',
          downBorderColor: '#00B050',
          noChangeBorderColor: '#888888',
          upWickColor: '#FF0000',
          downWickColor: '#00B050',
          noChangeWickColor: '#888888',
        },
      },
    });

    // 3. 设置精度
    chart.setPriceVolumePrecision(2, 0); // 价格 2 位，成交量 0 位

    // 4. 加载初始数据
    const mockData: KLineData[] = generateMockData();
    chart.applyNewData(mockData);

    // 5. 创建成交量副图（VOL）
    chart.createIndicator('VOL');

    // 6. 设置数据加载回调（向前/向后加载更多）
    chart.setLoadDataCallback(async (params) => {
      const { type, data, callback } = params;

      try {
        // 根据 type 判断是初始加载、向前加载（左拉）、还是向后加载（右拉）
        let moreData: KLineData[];
        let hasMore = false;

        if (type === 'init' || type === 'forward') {
          // 向前加载：获取更早的历史数据
          moreData = await fetchHistoricalData(symbol, period, data?.timestamp || 0, 'backward');
          hasMore = moreData.length > 0;
        } else if (type === 'backward') {
          // 向后加载：获取更新的数据
          moreData = await fetchHistoricalData(symbol, period, data?.timestamp || Date.now(), 'forward');
          hasMore = moreData.length > 0;
        } else {
          moreData = [];
        }

        // 调用回调函数，将新数据追加到图表
        callback(moreData, hasMore);
      } catch (error) {
        console.error('Failed to load data:', error);
        callback([], false);
      }
    });

    // 7. 清理
    return () => {
      if (chartRef.current) {
        dispose(chartRef.current);
        chartRef.current = null;
      }
    };
  }, [symbol, period]);

  return (
    <div
      ref={containerRef}
      style={{
        width: '100%',
        height: '600px', // 重要：必须明确设置高度，否则会塌陷
        backgroundColor: '#ffffff',
      }}
    />
  );
};

// ============ 辅助函数 ============

function generateMockData(): KLineData[] {
  const data: KLineData[] = [];
  let timestamp = Date.now() - 30 * 24 * 60 * 60 * 1000; // 30 days ago
  let close = 10;

  for (let i = 0; i < 30; i++) {
    const open = close;
    const high = open + Math.random() * 2;
    const low = open - Math.random() * 2;
    close = (high + low) / 2 + (Math.random() - 0.5);

    data.push({
      timestamp,
      open,
      high,
      low,
      close,
      volume: Math.floor(Math.random() * 1000000),
    });

    timestamp += 24 * 60 * 60 * 1000; // next day
  }

  return data;
}

async function fetchHistoricalData(
  symbol: string,
  period: string,
  referenceTimestamp: number,
  direction: 'forward' | 'backward'
): Promise<KLineData[]> {
  // TODO: 实现你的数据获取逻辑
  // 可以调用后端 API 或本地数据源

  // 示例：返回空数组（表示没有更多数据）
  return [];
}
```

---

## 核心 API 速查表

### 全局函数

| API | 签名 | 说明 |
|-----|------|------|
| `init()` | `init(dom: HTMLElement \| string, options?: Options) => Chart \| null` | 初始化图表。入参是 DOM 元素或选择器。 |
| `dispose()` | `dispose(dom: HTMLElement \| Chart \| string) => void` | 销毁图表，清理内存和事件监听。 |

### Chart 实例方法（核心数据操作）

| API | 签名 | 说明 |
|-----|------|------|
| `applyNewData()` | `applyNewData(dataList: KLineData[], more?: boolean, callback?: () => void) => void` | **推荐首选**。一次性加载完整数据集，y 轴自动拟合。`more=true` 表示有更多数据可加载。 |
| `updateData()` | `updateData(data: KLineData, callback?: () => void) => void` | 更新最后一根 K 线（用于实时行情）。 |
| `setLoadDataCallback()` | `setLoadDataCallback(cb: LoadDataCallback) => void` | 设置数据加载回调，用于支持左拉/右拉加载历史和新数据。 |
| `setStyles()` | `setStyles(styles: string \| DeepPartial<Styles>) => void` | 设置或更新样式。支持深度合并，不会覆盖未指定的字段。 |
| `getStyles()` | `getStyles() => Styles` | 获取当前样式对象。 |
| `setPriceVolumePrecision()` | `setPriceVolumePrecision(pricePrecision: number, volumePrecision: number) => void` | 设置价格和成交量的显示精度（小数位数）。 |
| `createIndicator()` | `createIndicator(name: string \| IndicatorCreate, isStack?: boolean, paneOptions?: PaneOptions, callback?: () => void) => string \| null` | 创建技术指标。返回指标 ID（如 `'VOL'`）。 |
| `setLocale()` | `setLocale(locale: string) => void` | 设置语言。常用：`'zh-CN'`（中文）、`'en'`（英文）。 |
| `getLocale()` | `getLocale() => string` | 获取当前语言设置。 |
| `setOffsetRightDistance()` | `setOffsetRightDistance(distance: number) => void` | 设置右侧空白距离（蜡烛图右边的留白）。 |

### Chart 实例方法（查询）

| API | 说明 |
|-----|------|
| `getTimezone()` / `setTimezone()` | 时区管理。 |
| `id` | 图表唯一标识。 |
| `getDom(paneId?: string, position?: string)` | 获取指定 pane 的 DOM 元素。 |

---

## 核心数据结构

### KLineData（必需字段）

```typescript
interface KLineData {
  timestamp: number;  // 时间戳（毫秒）
  open: number;       // 开盘价
  high: number;       // 最高价
  low: number;        // 最低价
  close: number;      // 收盘价
  volume?: number;    // 成交量（可选，但 VOL 指标需要）
  turnover?: number;  // 成交额（可选）
  [key: string]: any; // 其他自定义字段
}
```

### LoadDataParams（数据加载回调参数）

```typescript
interface LoadDataParams {
  type: LoadDataType;  // 'init' | 'forward' | 'backward'
  data: KLineData | null;  // 当前相关数据（init 时为 null）
  callback: (dataList: KLineData[], more?: boolean) => void;
}

enum LoadDataType {
  Init = 'init',           // 初始加载
  Forward = 'forward',     // 向前加载（左拉，历史数据）
  Backward = 'backward',   // 向后加载（右拉，新数据）
}
```

**关键点**：不要导入 `LoadDataType` 作为 enum——直接用字符串字面量 `'init' | 'forward' | 'backward'` 或检查 `params.type === 'forward'`。

---

## A 股配色配置

### 标准配置（红涨绿跌）

```typescript
const chineseStockStyle = {
  candle: {
    bar: {
      upColor: '#FF0000',          // 红（上涨）
      downColor: '#00B050',        // 绿（下跌）
      noChangeColor: '#888888',
      upBorderColor: '#FF0000',
      downBorderColor: '#00B050',
      noChangeBorderColor: '#888888',
      upWickColor: '#FF0000',
      downWickColor: '#00B050',
      noChangeWickColor: '#888888',
    },
  },
};

chart.setStyles(chineseStockStyle);
```

### 深色主题变体

```typescript
const darkTheme = {
  grid: {
    show: true,
    horizontal: {
      show: true,
      size: 1,
      color: '#3a3a3a',
      style: 'dashed',
      dashedValue: [2, 2],
    },
    vertical: {
      show: false,
    },
  },
  candle: {
    bar: {
      upColor: '#FF0000',
      downColor: '#00B050',
      noChangeColor: '#999999',
      upBorderColor: '#FF0000',
      downBorderColor: '#00B050',
      noChangeBorderColor: '#999999',
      upWickColor: '#FF0000',
      downWickColor: '#00B050',
      noChangeWickColor: '#999999',
    },
  },
};

chart.setStyles(darkTheme);
```

---

## 加载更多历史数据（左拉）

### 推荐模式：setLoadDataCallback + applyNewData

```typescript
chart.setLoadDataCallback(async (params) => {
  const { type, data, callback } = params;

  if (type === 'forward') {
    // 左拉：加载更早的历史数据
    // data 是当前图表最左边的 K 线
    const earlierData = await fetchData(
      data?.timestamp || Date.now(),
      'backward',
      50 // 一次加载 50 根 K 线
    );

    // 回调时将数据追加，第二参数 more=true 表示还有更多
    callback(earlierData, earlierData.length >= 50);
  } else if (type === 'backward') {
    // 右拉：加载更新的数据
    const newerData = await fetchData(data?.timestamp || Date.now(), 'forward', 50);
    callback(newerData, newerData.length >= 50);
  }
});

// 首次加载数据
const initialData = await fetchData(Date.now(), 'backward', 100);
chart.applyNewData(initialData, true); // more=true 启用左拉加载
```

### fetchData 实现示例

```typescript
async function fetchData(
  timestamp: number,
  direction: 'forward' | 'backward',
  count: number = 50
): Promise<KLineData[]> {
  // 调用后端 API
  const response = await fetch('/api/kline', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      symbol: '600000',
      period: '1d',
      timestamp,
      direction,
      count,
    }),
  });

  const data = await response.json();
  return data.klines || [];
}
```

---

## 成交量副图（VOL）

### 创建和配置

```typescript
// 创建默认 VOL（参数 [5, 10, 20]）
const volIndicatorId = chart.createIndicator('VOL');

// 自定义参数
const customVolId = chart.createIndicator({
  name: 'VOL',
  calcParams: [5, 10, 20], // 移动平均周期
});

// 配置 VOL 样式
chart.setStyles({
  indicator: {
    VOL: {
      bars: [
        {
          upColor: '#FF0000',
          downColor: '#00B050',
          noChangeColor: '#888888',
        },
      ],
    },
  },
});
```

---

## 内置指标清单

KLineChart 内置 **30+ 技术指标**。以下为常用指标：

### 主图覆盖指标（可在蜡烛图上叠加）

| 指标 | 默认参数 | 说明 |
|------|---------|------|
| MA | [5, 10, 30, 60] | 移动平均线 |
| EMA | [6, 12, 20] | 指数移动平均 |
| SMA | [10, 20, 30] | 简单移动平均 |
| BOLL | [20, 2] | 布林带 |
| SAR | [2, 2, 20] | 抛物线止损（SAR） |
| BBI | [3, 6, 12, 24] | 多空布林线 |

### 副图指标（单独窗口）

| 指标 | 默认参数 | 说明 |
|------|---------|------|
| VOL | [5, 10, 20] | 成交量 |
| MACD | [12, 26, 9] | MACD |
| KDJ | [9, 3, 3] | KDJ |
| RSI | [6, 12, 24] | 相对强弱指数 |
| CCI | [20] | 商品通道指数 |
| DMI | [14, 6] | 动向指数 |
| BIAS | [6, 12, 24] | 乖离率 |
| TRIX | [12] | 三重指数平滑 |
| ROC | [12] | 变化率 |
| OBV | [] | 能量潮 |
| PVT | [] | 价量趋势 |
| AO | [5, 34] | 奇妙振荡指标 |
| WR | [10, 6] | 威廉指标 |
| MTM | [12] | 动量指标 |
| EMV | [12, 9] | 简易波动指标 |
| AVP | [5, 3] | 平均价格指标 |
| CR | [26, 10, 20] | CR 指标 |
| PSY | [12] | 心理线指标 |
| DMA | [10, 5] | 快慢离差 |
| BRAR | [26] | BRAR 指标 |

### 创建指标示例

```typescript
// 创建副图指标
chart.createIndicator('MACD');
chart.createIndicator('KDJ');
chart.createIndicator('RSI');

// 在主图上叠加
chart.createIndicator('MA', true, { pane: { id: 'candle_pane' } });
chart.createIndicator('BOLL', true);

// 自定义参数
chart.createIndicator({
  name: 'MA',
  calcParams: [5, 10, 20], // 自定义周期
});
```

---

## 常见陷阱与解决方案

### 1. **LoadDataType 导入错误**

**问题**：
```typescript
import { LoadDataType } from 'klinecharts'; // ❌ SyntaxError!
if (params.type === LoadDataType.Forward) { ... }
```

**原因**：v9 中 `LoadDataType` 是内部 enum，不导出到顶层。

**解决方案**：
```typescript
// ✅ 直接用字符串字面量
if (params.type === 'forward') { ... }
if (params.type === 'backward') { ... }
if (params.type === 'init') { ... }
```

---

### 2. **Y 轴不自动拟合**

**问题**：使用 `setLoadDataCallback` + `callback()` 不能自动调整 y 轴。

**原因**：v10 beta 之前，`setLoadDataCallback` 不会触发 y 轴重新计算；需用 `applyNewData()`。

**解决方案**：
```typescript
// ✅ 推荐：使用 applyNewData 进行初始加载
const initialData = [...];
chart.applyNewData(initialData, true); // true 表示有更多数据

// ✅ 之后左拉/右拉时，用 setLoadDataCallback
chart.setLoadDataCallback(async (params) => {
  if (params.type === 'forward') {
    const moreData = await fetchData(...);
    params.callback(moreData, true);
  }
});
```

---

### 3. **容器高度塌陷为 0**

**问题**：图表不显示或只显示一条线。

**原因**：父容器没有明确高度，flexbox 或 grid 约束导致子容器高度计算为 0。

**解决方案**：
```typescript
// ✅ 明确设置容器高度
<div style={{ width: '100%', height: '600px' }}>
  {/* 图表会渲染在这里 */}
</div>

// ✅ 或在父容器上设置 display: flex + height
<div style={{ display: 'flex', height: '100vh' }}>
  <div ref={containerRef} style={{ flex: 1 }} /> {/* 自动拉伸 */}
</div>
```

---

### 4. **setStyles 浅合并问题**

**问题**：修改一个样式字段时，其他字段被覆盖。

**原因**：之前版本 `setStyles` 不做深度合并。

**解决方案**（v9 已修复，但要小心）：
```typescript
// ✅ v9.8.12 支持深度合并，只需传入要改的部分
chart.setStyles({
  candle: {
    bar: {
      upColor: '#FF0000', // 只改这个，其他字段保留
    },
  },
});

// ❌ 避免：一次性覆盖整个 styles 对象
const allStyles = chart.getStyles();
allStyles.candle.bar.upColor = '#FF0000';
chart.setStyles(allStyles); // 可能覆盖未指定的嵌套字段
```

---

### 5. **实时更新最后一根 K 线**

**问题**：不知道如何在新数据到达时更新最后一根 K 线。

**解决方案**：
```typescript
// ✅ 用 updateData 更新最后一根，不影响其他数据
const latestKLine: KLineData = {
  timestamp: Date.now(),
  open: 100,
  high: 105,
  low: 99,
  close: 102,
  volume: 1000000,
};

chart.updateData(latestKLine);
```

---

### 6. **销毁图表未清理事件**

**问题**：切换视图或卸载组件后，内存泄漏或事件仍在运行。

**解决方案**：
```typescript
// ✅ 必须调用 dispose
useEffect(() => {
  const chart = init(container);
  // ... 配置

  return () => {
    // 必须！否则事件和内存会累积
    dispose(chart);
    // 或者：dispose(container);
  };
}, []);
```

---

### 7. **中文显示不正常**

**问题**：坐标轴时间、提示框文字显示乱码或缺失。

**解决方案**：
```typescript
// ✅ 初始化时明确设置 locale
const chart = init(container, {
  locale: 'zh-CN',
});

// ✅ 或之后切换
chart.setLocale('zh-CN'); // 中文
chart.setLocale('en');    // 英文
```

---

### 8. **高精度价格显示错误**

**问题**：某些股票价格精度不对（如加密货币 0.0001 显示为 0）。

**解决方案**：
```typescript
// ✅ 调整精度，第一参数是小数位数
chart.setPriceVolumePrecision(4, 0); // 价格 4 位，成交量 0 位

// 常见配置：
chart.setPriceVolumePrecision(2, 0); // A 股标准（0.01）
chart.setPriceVolumePrecision(4, 2); // 加密货币（0.0001 价格，0.01 成交量）
```

---

## 升级 v10 的注意事项

v10.0.0-beta2 相比 v9.8.12 的关键改进和破坏性变更：

### 改进

1. **setDataLoader 修复**（v10 新）
   - v9 的 `setLoadDataCallback` 中 y 轴不自动拟合
   - v10 引入 `setDataLoader`，性能和 y 轴拟合更好
   - **迁移建议**：等 v10 正式发布后，逐步迁移

2. **API 简化**
   - 某些冗余 API 被移除（如 `loadMore`）
   - 建议查阅 v10 迁移指南

3. **性能优化**
   - Canvas 渲染优化
   - 内存占用减少

### 破坏性变更

1. **废弃 API**
   - `loadMore()` 已删除（v9 已废弃标注）
   - 改用 `setLoadDataCallback` / `setDataLoader`

2. **类型导出变化**
   - 某些内部 enum 可能不再导出
   - 用字符串字面量替代

3. **样式配置调整**
   - 部分 style 字段名可能变化
   - 需查看官方迁移指南

### 升级检查清单

```typescript
// ❌ v9 写法（可能在 v10 报错）
import { LoadDataType } from 'klinecharts';
chart.loadMore(...);

// ✅ v10 兼容写法（现在就可用）
if (params.type === 'forward') { ... }
chart.setLoadDataCallback(...);
```

**建议**：在正式发布前，继续用 v9；发布后启动升级计划，先在测试环境验证。

---

## 性能优化建议

1. **数据量控制**
   - 不要一次加载超过 5000 根 K 线
   - 用 `setLoadDataCallback` 分页加载

2. **指标数量**
   - 主图最多 3-5 个叠加指标
   - 副图不超过 2-3 个

3. **实时更新频率**
   - 如果用 `updateData`，限制更新频率（如 1 次/秒）
   - 避免 60+ fps 的数据流直接推送

4. **销毁清理**
   - 必须在组件卸载时调用 `dispose()`
   - 否则多个图表会共享事件、内存累积

---

## 常用工具函数集

### 格式化时间戳

```typescript
function formatTimestamp(timestamp: number, format = 'YYYY-MM-DD HH:mm'): string {
  const date = new Date(timestamp);
  const year = date.getFullYear();
  const month = String(date.getMonth() + 1).padStart(2, '0');
  const day = String(date.getDate()).padStart(2, '0');
  const hours = String(date.getHours()).padStart(2, '0');
  const minutes = String(date.getMinutes()).padStart(2, '0');

  return `${year}-${month}-${day} ${hours}:${minutes}`;
}
```

### 验证 KLineData 有效性

```typescript
function isValidKLineData(data: KLineData): boolean {
  return (
    typeof data.timestamp === 'number' &&
    typeof data.open === 'number' &&
    typeof data.high === 'number' &&
    typeof data.low === 'number' &&
    typeof data.close === 'number' &&
    data.high >= data.low &&
    data.open >= data.low &&
    data.open <= data.high &&
    data.close >= data.low &&
    data.close <= data.high
  );
}
```

### 计算 K 线颜色（用于自定义）

```typescript
function getCandleColor(
  open: number,
  close: number,
  style: 'chinese' | 'us' = 'chinese'
): { upColor: string; downColor: string } {
  if (style === 'chinese') {
    // 中国：红涨绿跌
    return {
      upColor: close > open ? '#FF0000' : '#00B050',
      downColor: close > open ? '#FF0000' : '#00B050',
    };
  } else {
    // 美国：绿涨红跌
    return {
      upColor: close > open ? '#00B050' : '#FF0000',
      downColor: close > open ? '#00B050' : '#FF0000',
    };
  }
}
```

---

## 官方资源

- **官网**：https://klinecharts.com
- **预览环境**：https://preview.klinecharts.com
- **Pro 版本**：https://pro.klinecharts.com
- **GitHub**：https://github.com/liihuu/KLineChart
- **示例项目**：https://github.com/liihuu/KLineChartSample

---

## 相关文件参考

- 项目 package.json：`klinecharts@^9.8.12`
- 类型定义：`node_modules/klinecharts/dist/index.d.ts`

---

*最后更新：2026-05-28*
*文档版本：v9.8.12 最佳实践*
