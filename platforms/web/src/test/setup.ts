// Test setup — registers @testing-library/jest-dom matchers (toBeInTheDocument,
// toHaveAttribute, …). The StreamCoalescer now falls back to setTimeout when
// rAF is absent (jsdom), so no rAF polyfill is needed here.
import '@testing-library/jest-dom/vitest'
