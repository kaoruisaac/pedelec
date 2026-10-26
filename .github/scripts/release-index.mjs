import { execFileSync } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const official = (release) => release.draft === false && release.prerelease === false;

export function buildIndex(releases, latest) {
  const published = releases.filter(official);
  if (!published.length) return null;
  if (!latest || !official(latest) || !published.some((r) => r.id === latest.id && r.tag_name === latest.tag_name)) {
    throw new Error('GitHub latest must be a public non-prerelease in the release list. Retry rebuilding.');
  }
  for (const release of published) {
    if (typeof release.tag_name !== 'string' || !release.tag_name.startsWith('v') ||
        typeof release.published_at !== 'string' || !Number.isFinite(Date.parse(release.published_at))) {
      throw new Error('Invalid public release tag or publication date.');
    }
  }
  const byDate = (a, b) => Date.parse(b.published_at) - Date.parse(a.published_at) || b.id - a.id;
  const selected = published.sort(byDate).slice(0, 50);
  // GitHub's explicit latest choice can be older than the 50 newest publications.
  if (!selected.some((r) => r.id === latest.id)) selected[selected.length - 1] = latest;
  return {
    schemaVersion: 1,
    latest: latest.tag_name,
    releases: selected.sort(byDate).map((r) => ({
      version: r.tag_name,
      publishedAt: new Date(r.published_at).toISOString().replace('.000Z', 'Z'),
      title: r.name || r.tag_name,
    })),
  };
}

export function publishIndex({ repository = process.env.GITHUB_REPOSITORY, run = execFileSync, outputDirectory = 'dist' } = {}) {
  if (!/^[\w.-]+\/[\w.-]+$/.test(repository ?? '')) throw new Error('Invalid GITHUB_REPOSITORY.');
  const gh = (args) => run('gh', args, {
    encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'], maxBuffer: 64 * 1024 * 1024,
  });
  const pages = JSON.parse(gh(['api', `repos/${repository}/releases?per_page=100`, '--paginate', '--slurp']));
  if (!Array.isArray(pages) || !pages.every(Array.isArray)) throw new Error('Invalid release list response.');
  const releases = pages.flat();
  if (!releases.some(official)) {
    console.log('No public formal releases; no index asset to upload.');
    return null;
  }
  const getLatest = () => JSON.parse(gh(['api', `repos/${repository}/releases/latest`]));
  const latest = getLatest();
  const index = buildIndex(releases, latest);
  mkdirSync(outputDirectory, { recursive: true });
  const asset = resolve(outputDirectory, 'releases.json');
  writeFileSync(asset, `${JSON.stringify(index, null, 2)}\n`);
  const current = getLatest();
  if (current.id !== latest.id || current.tag_name !== latest.tag_name || !official(current)) {
    throw new Error('GitHub latest changed during rebuild. Retry the workflow.');
  }
  // Only this named asset is replaced; no release or installer assets are removed.
  gh(['release', 'upload', latest.tag_name, asset, '--repo', repository, '--clobber']);
  return index;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) publishIndex();
