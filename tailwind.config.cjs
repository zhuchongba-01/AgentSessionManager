/** @type {import('tailwindcss').Config} */
module.exports = {
  content: ["./src/index.html", "./src/**/*.{ts,tsx}"],
  darkMode: ["selector", ".dark"],
  theme: {
    extend: {
      colors: {
        background: "hsl(var(--background))",
        foreground: "hsl(var(--foreground))",
        card: {
          DEFAULT: "hsl(var(--card))",
          foreground: "hsl(var(--card-foreground))",
        },
        popover: {
          DEFAULT: "hsl(var(--popover))",
          foreground: "hsl(var(--popover-foreground))",
        },
        primary: {
          DEFAULT: "hsl(var(--primary))",
          foreground: "hsl(var(--primary-foreground))",
        },
        secondary: {
          DEFAULT: "hsl(var(--secondary))",
          foreground: "hsl(var(--secondary-foreground))",
        },
        muted: {
          DEFAULT: "hsl(var(--muted))",
          foreground: "hsl(var(--muted-foreground))",
        },
        accent: {
          DEFAULT: "hsl(var(--accent))",
          foreground: "hsl(var(--accent-foreground))",
        },
        destructive: {
          DEFAULT: "hsl(var(--destructive))",
          foreground: "hsl(var(--destructive-foreground))",
        },
        border: "hsl(var(--border))",
        "border-strong": "hsl(var(--border-strong))",
        strong: "hsl(var(--border-strong))",
        input: "hsl(var(--input))",
        ring: "hsl(var(--ring))",
        app: "hsl(var(--bg-app))",
        surface: "hsl(var(--bg-surface))",
        subtle: "var(--bg-subtle)",
        selected: "var(--bg-selected)",
        "fg-1": "hsl(var(--fg-1))",
        "fg-2": "hsl(var(--fg-2))",
        "fg-3": "hsl(var(--fg-3))",
        action: {
          DEFAULT: "hsl(var(--action-bg))",
          text: "hsl(var(--action-text))",
        },
        success: {
          DEFAULT: "hsl(var(--success-border))",
          text: "hsl(var(--success-text))",
        },
        danger: {
          DEFAULT: "hsl(var(--destructive))",
          text: "hsl(var(--danger-text))",
        },
        warning: {
          DEFAULT: "hsl(var(--warning-border))",
          soft: "var(--warning-soft)",
          text: "hsl(var(--warning-text))",
        },
        "diff-add": "var(--diff-add-bg)",
        "diff-del": "var(--diff-del-bg)",
        blue: {
          400: "#409CFF",
          500: "#0A84FF",
          600: "#0060DF",
        },
        gray: {
          50: "#fafafa",
          100: "#f4f4f5",
          200: "#e4e4e7",
          300: "#d4d4d8",
          400: "#a1a1aa",
          500: "#71717a",
          600: "#636366",
          700: "#48484A",
          800: "#3A3A3C",
          900: "#2C2C2E",
          950: "#1C1C1E",
        },
        green: {
          100: "#d1fae5",
          500: "#10b981",
        },
        red: {
          100: "#fee2e2",
          500: "#ef4444",
        },
        amber: {
          100: "#fef3c7",
          500: "#f59e0b",
        },
      },
      boxShadow: {
        sm: "0 1px 2px 0 rgb(0 0 0 / 0.05)",
        md: "0 4px 6px -1px rgb(0 0 0 / 0.1), 0 2px 4px -2px rgb(0 0 0 / 0.1)",
        lg: "0 10px 15px -3px rgb(0 0 0 / 0.1), 0 4px 6px -4px rgb(0 0 0 / 0.1)",
        "v7-md":
          "0 8px 24px -4px rgb(15 23 42 / 0.12), 0 2px 6px -2px rgb(15 23 42 / 0.08)",
        "v7-lg":
          "0 18px 42px -8px rgb(15 23 42 / 0.2), 0 4px 12px -4px rgb(15 23 42 / 0.1)",
      },
      borderRadius: {
        sm: "0.375rem",
        md: "0.5rem",
        lg: "0.75rem",
        xl: "0.875rem",
        control: "0.375rem",
        panel: "0.625rem",
        dialog: "0.75rem",
      },
      fontSize: {
        title: ["1rem", { lineHeight: "1.5rem", fontWeight: "600" }],
        section: ["0.875rem", { lineHeight: "1.25rem", fontWeight: "600" }],
        body: ["0.875rem", { lineHeight: "1.4rem" }],
        caption: ["0.75rem", { lineHeight: "1.125rem" }],
        badge: ["0.6875rem", { lineHeight: "1rem", fontWeight: "500" }],
      },
      fontFamily: {
        // 使用与之前版本保持一致的系统字体栈
        sans: [
          '"Inter"',
          '"Segoe UI Variable Text"',
          '"Segoe UI"',
          "-apple-system",
          "BlinkMacSystemFont",
          '"PingFang SC"',
          '"Microsoft YaHei UI"',
          '"Noto Sans SC"',
          "sans-serif",
        ],
        mono: [
          '"JetBrains Mono"',
          '"Cascadia Code"',
          '"Cascadia Mono"',
          "ui-monospace",
          "SFMono-Regular",
          "Consolas",
          "Menlo",
          "monospace",
        ],
      },
      animation: {
        "fade-in": "fadeIn 0.5s ease-out",
        "slide-up": "slideUp 0.5s ease-out",
        "slide-down": "slideDown 0.3s ease-out",
        "slide-in-right": "slideInRight 0.3s ease-out",
        "pulse-slow": "pulse 3s cubic-bezier(0.4, 0, 0.6, 1) infinite",
        "accordion-down": "accordion-down 0.2s ease-out",
        "accordion-up": "accordion-up 0.2s ease-out",
      },
      keyframes: {
        fadeIn: {
          "0%": {
            opacity: "0",
          },
          "100%": {
            opacity: "1",
          },
        },
        slideUp: {
          "0%": {
            transform: "translateY(20px)",
            opacity: "0",
          },
          "100%": {
            transform: "translateY(0)",
            opacity: "1",
          },
        },
        slideDown: {
          "0%": {
            transform: "translateY(-100%)",
            opacity: "0",
          },
          "100%": {
            transform: "translateY(0)",
            opacity: "1",
          },
        },
        slideInRight: {
          "0%": {
            transform: "translateX(100%)",
            opacity: "0",
          },
          "100%": {
            transform: "translateX(0)",
            opacity: "1",
          },
        },
        "accordion-down": {
          from: {
            height: "0",
          },
          to: {
            height: "var(--radix-accordion-content-height)",
          },
        },
        "accordion-up": {
          from: {
            height: "var(--radix-accordion-content-height)",
          },
          to: {
            height: "0",
          },
        },
      },
    },
  },
  plugins: [],
};
