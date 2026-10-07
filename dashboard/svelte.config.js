import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

export default {
  preprocess: vitePreprocess(),
  compilerOptions: {
    // Component styles go to the CSS file, never into a <style> the page
    // would need 'unsafe-inline' for.
    css: 'external',
  },
};
