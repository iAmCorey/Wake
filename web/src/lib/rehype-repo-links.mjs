// The docs and the changelog are rendered straight from the repo's Markdown, whose
// links are written for GitHub (relative paths like `cli.md#refresh`). This rewrites
// them for the site: the two docs that have pages here point at those pages, anything
// else in the repo points at GitHub.
const REPO = 'https://github.com/iAmCorey/Wake';
const PAGES = { 'mcp.md': '/docs/mcp', 'cli.md': '/docs/cli', 'CHANGELOG.md': '/changelog' };

function rewrite(href, file) {
  if (/^(?:[a-z]+:|#|\/)/i.test(href)) return href;
  const [target, hash = ''] = href.split('#');
  const name = target.split('/').pop();
  if (PAGES[name]) return PAGES[name] + (hash ? `#${hash}` : '');
  // Resolve against the Markdown file's own directory inside the repo.
  const base = file && file.includes('/docs/') ? 'docs/' : '';
  const parts = (base + target).split('/');
  const out = [];
  for (const p of parts) {
    if (p === '..') out.pop();
    else if (p && p !== '.') out.push(p);
  }
  return `${REPO}/blob/main/${out.join('/')}${hash ? `#${hash}` : ''}`;
}

export function rewriteRepoLinks() {
  return (tree, file) => {
    const visit = (node) => {
      if (node.type === 'element' && node.tagName === 'a' && typeof node.properties?.href === 'string') {
        node.properties.href = rewrite(node.properties.href, file?.path ?? '');
      }
      node.children?.forEach(visit);
    };
    visit(tree);
  };
}
