// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import { cheeselordTheme } from '@cheeselord/design/starlight';

export default defineConfig({
  site: 'https://paulnsorensen.github.io',
  base: '/hallouminate',
  output: 'static',
  integrations: [
    starlight({
      title: '🧀 hallouminate',
      plugins: [cheeselordTheme({ flavor: 'hallouminate' })],
      description: 'Persistent, repo-local knowledge for coding agents.',
      sidebar: [
        { label: 'Introduction', slug: 'intro' },
        { label: 'Installation', slug: 'install' },
        { label: 'How it compares', slug: 'comparison' },
        { label: 'CLI reference', slug: 'cli' },
        { label: 'MCP surface', slug: 'mcp' },
        { label: 'Configuration', slug: 'config' },
        { label: 'Architecture', slug: 'architecture' },
        { label: 'Development verification', slug: 'development-verification' },
        { label: 'Releasing crates', slug: 'releasing' },
        { label: 'Dogfooding: our own wiki', slug: 'dogfooding' },
      ],
      components: {
        SiteTitle: './src/components/SiteTitle.astro',
      },
      favicon: '/favicon.png',
      social: [
        {
          icon: 'github',
          label: 'GitHub',
          href: 'https://github.com/paulnsorensen/hallouminate',
        },
      ],
      editLink: {
        baseUrl: 'https://github.com/paulnsorensen/hallouminate/edit/main/site/src/content/docs/',
      },
    }),
  ],
});
