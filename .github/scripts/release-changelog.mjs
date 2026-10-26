import { readFileSync, realpathSync, statSync, mkdirSync, writeFileSync } from 'node:fs';
import { resolve, relative, isAbsolute } from 'node:path';
import { pathToFileURL } from 'node:url';

export function validateChangelog(tag, root = process.cwd()) {
  if (typeof tag !== 'string' || tag !== tag.trim() ||
      !/^v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(tag)) {
    throw new Error('Release tag must be v<version> without path separators.');
  }
  const directory = resolve(root, 'changelog');
  const source = resolve(directory, `${tag}.md`);
  // Resolve symlinks too: neither the directory nor the file may escape changelog/.
  const actual = realpathSync(source);
  const inside = relative(directory, actual);
  if (inside.startsWith('..') || isAbsolute(inside) || !statSync(actual).isFile()) {
    throw new Error('Changelog must be a regular file inside changelog/.');
  }
  const bytes = readFileSync(source);
  const markdown = new TextDecoder('utf-8', { fatal: true }).decode(bytes);
  if (!markdown.trim()) throw new Error(`Empty changelog: changelog/${tag}.md`);
  return bytes;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const bytes = validateChangelog(process.env.TAG_NAME);
  if (process.argv.includes('--stage')) {
    mkdirSync('dist', { recursive: true });
    writeFileSync('dist/changelog.md', bytes);
  }
  console.log(`Validated changelog/${process.env.TAG_NAME}.md (${bytes.length} bytes)`);
}
