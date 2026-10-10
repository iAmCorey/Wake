// The docs pages render the repo's own Markdown, so the site never carries a second copy.
import { defineCollection } from 'astro:content';
import { glob } from 'astro/loaders';

const docs = defineCollection({
  loader: glob({ pattern: ['mcp.md', 'cli.md'], base: '../docs' }),
});

export const collections = { docs };
