export default {
  test: {
    environment: "node",
    include: ["tests/**/*.test.ts"],
    coverage: {
      provider: "v8",
      reporter: ["text", "lcov"],
      include: ["src/**/*.ts"],
      exclude: ["src/cli/main.ts", "src/index.ts", "src/testing/**"],
      thresholds: {
        statements: 98,
        branches: 98,
        functions: 98,
        lines: 98
      }
    }
  }
};
