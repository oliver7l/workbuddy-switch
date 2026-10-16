// macOS 11.7 兼容：Tailwind v3 + autoprefixer（替代 @tailwindcss/vite 的 v4 管线）。
// autoprefixer 负责 -webkit- 前缀，覆盖 Safari 14 需要的 backdrop-filter / appearance 等。
export default {
  plugins: {
    tailwindcss: {},
    autoprefixer: {},
  },
};
