import path from 'node:path';
import { fileURLToPath } from 'node:url';

import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';

const root = path.dirname(fileURLToPath(import.meta.url));

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5174,
    strictPort: true,
    host: true,
  },
  preview: {
    port: 5174,
    strictPort: true,
    host: true,
  },
  build: {
    chunkSizeWarningLimit: 6000,
  },
  resolve: {
    alias: {
      '@devolutions/iron-remote-desktop': path.resolve(
        root,
        '../iron-remote-desktop/dist/iron-remote-desktop.js',
      ),
      '@devolutions/iron-remote-desktop-rdp': path.resolve(
        root,
        '../iron-remote-desktop-rdp/dist/iron-remote-desktop-rdp.js',
      ),
    },
  },
  optimizeDeps: {
    exclude: ['@devolutions/iron-remote-desktop', '@devolutions/iron-remote-desktop-rdp'],
  },
});
