import { defineConfig } from "vitest/config";

// 最小 vitest 配置：纯函数测试用默认 node 环境；DOM 测试（scrollAnchor）
// 通过文件顶部 `// @vitest-environment jsdom` 注释单独切到 jsdom，避免全局拖慢。
export default defineConfig({
  test: {
    include: ["src/**/*.test.ts"],
  },
});
