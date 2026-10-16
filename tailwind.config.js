/*
 * macOS 11.7（Safari 14 WebView）兼容配置。
 * 颜色全部走 CSS 变量（src/index.css 的 :root / .dark），不透明色用
 * `rgb(var(--x-rgb) / <alpha-value>)` 通道形式以支持 bg-primary/5 这类透明度修饰符；
 * 深色主题下带 alpha 的 --border/--input/--sidebar-border 用完整 rgba() 形式
 * （修饰符在这些颜色上退化为不透明，可接受）。
 * darkMode: "class" 对应 v4 的 `@custom-variant dark (&:is(.dark *))`。
 */
/** @type {import('tailwindcss').Config} */
export default {
  darkMode: "class",
  content: ["./index.html", "./src/**/*.{ts,tsx,js,jsx}"],
  theme: {
    extend: {
      colors: {
        border: "var(--border)",
        input: "var(--input)",
        ring: "rgb(var(--ring-rgb) / <alpha-value>)",
        background: "rgb(var(--background-rgb) / <alpha-value>)",
        foreground: "rgb(var(--foreground-rgb) / <alpha-value>)",
        primary: {
          DEFAULT: "rgb(var(--primary-rgb) / <alpha-value>)",
          foreground: "rgb(var(--primary-foreground-rgb) / <alpha-value>)",
        },
        secondary: {
          DEFAULT: "rgb(var(--secondary-rgb) / <alpha-value>)",
          foreground: "rgb(var(--secondary-foreground-rgb) / <alpha-value>)",
        },
        destructive: {
          DEFAULT: "rgb(var(--destructive-rgb) / <alpha-value>)",
          foreground: "rgb(var(--destructive-foreground-rgb) / <alpha-value>)",
        },
        muted: {
          DEFAULT: "rgb(var(--muted-rgb) / <alpha-value>)",
          foreground: "rgb(var(--muted-foreground-rgb) / <alpha-value>)",
        },
        accent: {
          DEFAULT: "rgb(var(--accent-rgb) / <alpha-value>)",
          foreground: "rgb(var(--accent-foreground-rgb) / <alpha-value>)",
        },
        popover: {
          DEFAULT: "rgb(var(--popover-rgb) / <alpha-value>)",
          foreground: "rgb(var(--popover-foreground-rgb) / <alpha-value>)",
        },
        card: {
          DEFAULT: "rgb(var(--card-rgb) / <alpha-value>)",
          foreground: "rgb(var(--card-foreground-rgb) / <alpha-value>)",
        },
        brand: {
          DEFAULT: "rgb(var(--brand-rgb) / <alpha-value>)",
          foreground: "rgb(var(--brand-foreground-rgb) / <alpha-value>)",
        },
        chart: {
          1: "rgb(var(--chart-1-rgb) / <alpha-value>)",
          2: "rgb(var(--chart-2-rgb) / <alpha-value>)",
          3: "rgb(var(--chart-3-rgb) / <alpha-value>)",
          4: "rgb(var(--chart-4-rgb) / <alpha-value>)",
          5: "rgb(var(--chart-5-rgb) / <alpha-value>)",
        },
        sidebar: {
          DEFAULT: "rgb(var(--sidebar-rgb) / <alpha-value>)",
          foreground: "rgb(var(--sidebar-foreground-rgb) / <alpha-value>)",
          primary: "rgb(var(--sidebar-primary-rgb) / <alpha-value>)",
          "primary-foreground": "rgb(var(--sidebar-primary-foreground-rgb) / <alpha-value>)",
          accent: "rgb(var(--sidebar-accent-rgb) / <alpha-value>)",
          "accent-foreground": "rgb(var(--sidebar-accent-foreground-rgb) / <alpha-value>)",
          border: "var(--sidebar-border)",
          ring: "rgb(var(--sidebar-ring-rgb) / <alpha-value>)",
        },
      },
      borderRadius: {
        sm: "calc(var(--radius) - 4px)",
        md: "calc(var(--radius) - 2px)",
        lg: "var(--radius)",
        xl: "calc(var(--radius) + 4px)",
      },
    },
  },
  plugins: [],
};
