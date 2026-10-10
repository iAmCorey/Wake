// Download links, resolved once per build. The GitHub release is the source of truth:
// asset names carry the version, so there is no stable "latest" URL to hard-code.
// When the API is unreachable (rate limit, offline build) the links are built from the
// workspace version with the release workflow's naming scheme instead.
import { readFile } from 'node:fs/promises';
import path from 'node:path';

const REPO = 'iAmCorey/Wake';
export const REPO_URL = `https://github.com/${REPO}`;

export interface Release {
  version: string;
  date: string | null;
  url: string;
  mac: string;
  linux: { deb: { arm64: string; amd64: string }; tar: { arm64: string; amd64: string } };
  windows: string;
  stars: number | null;
}

async function gh(endpoint: string): Promise<any | null> {
  const headers: Record<string, string> = { Accept: 'application/vnd.github+json', 'User-Agent': 'wake-site' };
  if (process.env.GITHUB_TOKEN) headers.Authorization = `Bearer ${process.env.GITHUB_TOKEN}`;
  try {
    const res = await fetch(`https://api.github.com/repos/${REPO}${endpoint}`, { headers, signal: AbortSignal.timeout(8000) });
    return res.ok ? await res.json() : null;
  } catch {
    return null;
  }
}

async function workspaceVersion(): Promise<string> {
  // Builds run from site/ (the bundled module no longer sits next to its source).
  const toml = await readFile(path.resolve(process.cwd(), '../Cargo.toml'), 'utf8');
  const m = toml.match(/^version\s*=\s*"([^"]+)"/m);
  if (!m) throw new Error('No version in Cargo.toml');
  return m[1];
}

function fromNames(version: string, find: (name: string) => string | undefined): Release {
  const dl = (name: string) => find(name) ?? `${REPO_URL}/releases/download/v${version}/${name}`;
  return {
    version,
    date: null,
    url: `${REPO_URL}/releases/tag/v${version}`,
    mac: dl(`Wake-${version}-macos.zip`),
    linux: {
      deb: { arm64: dl(`wake_${version}_arm64.deb`), amd64: dl(`wake_${version}_amd64.deb`) },
      tar: { arm64: dl(`wake-${version}-linux-arm64.tar.gz`), amd64: dl(`wake-${version}-linux-amd64.tar.gz`) },
    },
    windows: dl(`wake-${version}-windows-x86_64.zip`),
    stars: null,
  };
}

let cached: Promise<Release> | undefined;

export function getRelease(): Promise<Release> {
  cached ??= (async () => {
    const [releases, repo] = await Promise.all([gh('/releases?per_page=6'), gh('')]);
    // A release whose packages are still being built has fewer than six assets; the
    // newest complete one is what a download button should point at meanwhile.
    const latest = Array.isArray(releases)
      ? releases.find((r: any) => !r.draft && !r.prerelease && Array.isArray(r.assets) && r.assets.length >= 6)
      : undefined;
    let release: Release;
    if (latest?.tag_name) {
      const byName = new Map<string, string>(latest.assets.map((a: any) => [a.name, a.browser_download_url]));
      release = fromNames(String(latest.tag_name).replace(/^v/, ''), (n) => byName.get(n));
      release.date = latest.published_at ?? null;
      release.url = latest.html_url ?? release.url;
    } else {
      release = fromNames(await workspaceVersion(), () => undefined);
    }
    release.stars = typeof repo?.stargazers_count === 'number' ? repo.stargazers_count : null;
    return release;
  })();
  return cached;
}
