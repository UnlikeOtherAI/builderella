import react from '@vitejs/plugin-react';
import { loadEnv } from 'vite';
import { defineConfig } from 'vitest/config';

// The controller embeds the built files and serves them at `/`. Asset paths
// are relative so the bundle also works behind a path prefix.
export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), ['VITE_', 'BELLA_']);
  // Where `npm run dev` forwards `/api`. Plain HTTP on loopback is the
  // controller's default for local development.
  const controller = env.BELLA_CONTROLLER_URL || 'http://127.0.0.1:7420';
  return {
    base: './',
    plugins: [react()],
    // No source maps: the files are embedded in the controller binary.
    build: { outDir: 'dist', sourcemap: false },
    server: {
      proxy: {
        '/api': { target: controller, changeOrigin: true, secure: false },
      },
    },
    test: {
      environment: 'jsdom',
      include: ['src/**/*.test.{ts,tsx}'],
      restoreMocks: true,
    },
  };
});
