// Copies the brand marks and the app icon from the app's own assets into public/,
// resized for the web. The app is the single source: nothing here is checked in.
import { mkdir, readdir, copyFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import sharp from 'sharp';

const root = path.resolve(fileURLToPath(new URL('..', import.meta.url)));
const assets = path.resolve(root, '../crates/wake/assets');
const pub = path.join(root, 'public');

await mkdir(path.join(pub, 'brands'), { recursive: true });

const brands = (await readdir(path.join(assets, 'brands'))).filter((f) => f.endsWith('.png'));
await Promise.all(
  brands.map((file) =>
    sharp(path.join(assets, 'brands', file))
      .resize(96, 96, { fit: 'contain', background: { r: 0, g: 0, b: 0, alpha: 0 } })
      .webp({ quality: 90 })
      .toFile(path.join(pub, 'brands', file.replace(/\.png$/, '.webp'))),
  ),
);

const svg = path.join(assets, 'icon.svg');
await copyFile(svg, path.join(pub, 'favicon.svg'));
for (const size of [180, 512]) {
  await sharp(svg, { density: 300 }).resize(size, size).png().toFile(path.join(pub, `icon-${size}.png`));
}

console.log(`sync-assets: ${brands.length} brand marks, app icon`);
