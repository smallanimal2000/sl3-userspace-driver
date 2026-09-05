import { defineConfig } from 'vite';

// Serves index.html + main.js + the wasm-pack output over one localhost origin
// (a secure context, which WebUSB requires) with correct MIME types, avoiding the
// ES-module / .wasm CORS errors you hit with a plain static file server.
export default defineConfig({
  root: '.',
  server: { port: 5173, open: true },
  build: { target: 'esnext', outDir: 'dist' },
});
