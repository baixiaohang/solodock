import { svelte } from '@sveltejs/vite-plugin-svelte'
import { defineConfig } from 'vite'

// https://vite.dev/config/
export default defineConfig({
  plugins: [svelte()],
  resolve: {
    conditions: ['browser'],
  },
  server: {
    proxy: {
      '/api': process.env.SOLODOCK_API_ORIGIN ?? 'http://127.0.0.1:8080',
      '/healthz': process.env.SOLODOCK_API_ORIGIN ?? 'http://127.0.0.1:8080',
    },
  },
})
