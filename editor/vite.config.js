import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vite';

export default defineConfig({
  plugins: [svelte()],
  server: {
    host: '127.0.0.1',
    port: 5173,
    strictPort: false,
    hmr: {
      protocol: 'ws',
      host: '127.0.0.1'
    },
    proxy: {
      '/api': {
        target: process.env.VELOGATE_EDITOR_API ?? 'http://127.0.0.1:3000',
        changeOrigin: true,
        ws: true
      }
    }
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    rollupOptions: {
      output: {
        manualChunks(id) {
          if (id.includes('elkjs')) return 'elk';
          if (id.includes('@xyflow')) return 'xyflow';
          if (id.includes('node_modules')) return 'vendor';
        }
      }
    }
  }
});
