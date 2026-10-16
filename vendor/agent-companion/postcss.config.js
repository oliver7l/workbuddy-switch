// 本目录的 postcss 配置：只留 autoprefixer（Tailwind 4 由 @tailwindcss/vite 插件处理）。
// 另一个作用：阻止 vite 向上找到主项目的 postcss.config.js——主项目为 macOS 11.7 兼容
// 已降级到 Tailwind v3，其 postcss 配置会把这里的 v4 语法（@import "tailwindcss"）处理崩。
export default {
  plugins: {
    autoprefixer: {},
  },
};
