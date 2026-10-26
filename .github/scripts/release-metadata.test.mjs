import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync, symlinkSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { execFileSync } from 'node:child_process';
import { validateChangelog } from './release-changelog.mjs';
import { buildIndex, publishIndex } from './release-index.mjs';

const release = (id, extra = {}) => ({
  id, tag_name: `v1.0.${id}`, name: `Pedelec v1.0.${id}`,
  published_at: new Date(Date.UTC(2026, 0, id)).toISOString(),
  draft: false, prerelease: false, ...extra,
});
function temporary(t) {
  const directory = mkdtempSync(join(tmpdir(), 'pedelec-release-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  return directory;
}

for (const count of [0, 1, 50, 51]) {
  test(`index with ${count} formal releases`, () => {
    const releases = Array.from({ length: count }, (_, i) => release(i + 1));
    const index = buildIndex(releases, releases.at(-1));
    if (!count) return assert.equal(index, null);
    assert.equal(index.schemaVersion, 1);
    assert.equal(index.latest, `v1.0.${count}`);
    assert.equal(index.releases.length, Math.min(count, 50));
    assert.deepEqual(index.releases.map((r) => r.version),
      Array.from({ length: Math.min(count, 50) }, (_, i) => `v1.0.${count - i}`));
    assert.equal(index.releases[0].publishedAt, releases.at(-1).published_at.replace('.000Z', 'Z'));
    assert.equal(index.releases[0].title, releases.at(-1).name);
  });
}

test('exclude drafts/prereleases, sort by publication date rather than version or API order', () => {
  const olderVersionPublishedLater = release(1, { published_at: '2026-05-01T08:00:00+08:00', name: null });
  const index = buildIndex([release(2), release(4, { draft: true }), olderVersionPublishedLater,
    release(5, { prerelease: true })], release(2));
  assert.equal(index.latest, 'v1.0.2');
  assert.deepEqual(index.releases, [
    { version: 'v1.0.1', publishedAt: '2026-05-01T00:00:00Z', title: 'v1.0.1' },
    { version: 'v1.0.2', publishedAt: '2026-01-02T00:00:00Z', title: 'Pedelec v1.0.2' },
  ]);
});

test('reserve a slot for GitHub latest even outside the newest 50', () => {
  const releases = Array.from({ length: 51 }, (_, i) => release(i + 1));
  const index = buildIndex(releases, releases[0]);
  assert.equal(index.latest, 'v1.0.1');
  assert.equal(index.releases.length, 50);
  assert.equal(index.releases.at(-1).version, index.latest);
  assert.ok(!index.releases.some((r) => r.version === 'v1.0.2'));
});

test('reject inconsistent latest and invalid metadata', () => {
  for (const latest of [null, release(2), release(1, { draft: true }), release(1, { prerelease: true })]) {
    assert.throws(() => buildIndex([release(1)], latest));
  }
  for (const extra of [{ published_at: null }, { published_at: 'invalid' }, { tag_name: '1.0.1' }]) {
    assert.throws(() => buildIndex([release(1, extra)], release(1, extra)));
  }
});

function fakeGitHub(t, { pages = [[release(1)]], failAt, changedLatest } = {}) {
  const calls = [];
  let latestReads = 0;
  const options = {
    repository: 'owner/repo', outputDirectory: temporary(t),
    run(command, args) {
      assert.equal(command, 'gh');
      calls.push(args);
      if (calls.length === failAt) throw new Error('GitHub unavailable');
      if (args[0] === 'release') {
        assert.deepEqual(args.slice(0, 3), ['release', 'upload', 'v1.0.1']);
        assert.deepEqual(args.slice(4), ['--repo', 'owner/repo', '--clobber']);
        assert.equal(JSON.parse(readFileSync(args[3], 'utf8')).latest, 'v1.0.1');
        return '';
      }
      if (args[1].endsWith('/latest')) return JSON.stringify(++latestReads === 2 && changedLatest ? changedLatest : release(1));
      assert.deepEqual(args, ['api', 'repos/owner/repo/releases?per_page=100', '--paginate', '--slurp']);
      return JSON.stringify(pages);
    },
  };
  return { calls, options };
}

test('pagination includes all pages; repeated runs replace only releases.json', (t) => {
  const { calls, options } = fakeGitHub(t, { pages: [[release(2, { draft: true })], [release(1)]] });
  const first = publishIndex(options);
  const bytes = readFileSync(join(options.outputDirectory, 'releases.json'));
  assert.deepEqual(publishIndex(options), first);
  assert.deepEqual(readFileSync(join(options.outputDirectory, 'releases.json')), bytes);
  assert.equal(calls.filter((args) => args[0] === 'release').length, 2);
  assert.ok(calls.every((args) => !args.includes('delete')));
});

test('zero formal releases skips latest API and upload', (t) => {
  const { options, calls } = fakeGitHub(t, { pages: [[release(1, { draft: true }), release(2, { prerelease: true })]] });
  assert.equal(publishIndex(options), null);
  assert.equal(calls.length, 1);
});

for (const failAt of [1, 2, 3, 4]) {
  test(`API/upload failure at call ${failAt} fails the run`, (t) => {
    const { options, calls } = fakeGitHub(t, { failAt });
    assert.throws(() => publishIndex(options), /GitHub unavailable/);
    assert.equal(calls.length, failAt);
  });
}

test('latest changing during rebuild aborts before upload', (t) => {
  const { options, calls } = fakeGitHub(t, { changedLatest: release(3) });
  assert.throws(() => publishIndex(options), /latest changed/);
  assert.ok(calls.every((args) => args[0] === 'api'));
});

test('missing, mismatched, empty, non-UTF8 and unsafe changelogs fail', (t) => {
  const root = temporary(t);
  mkdirSync(join(root, 'changelog'));
  for (const tag of [undefined, '../v1.0.1', 'v1.0.1/../x', 'v1.0.1\\x', '1.0.1', 'v1.0.1\n']) {
    assert.throws(() => validateChangelog(tag, root));
  }
  assert.throws(() => validateChangelog('v1.0.1', root));
  writeFileSync(join(root, 'changelog/v1.0.2.md'), '# Another version');
  assert.throws(() => validateChangelog('v1.0.1', root));
  for (const bytes of ['', ' \r\n\t', Buffer.from([0xff])]) {
    writeFileSync(join(root, 'changelog/v1.0.1.md'), bytes);
    assert.throws(() => validateChangelog('v1.0.1', root));
  }
});

test('symlinked changelog directory cannot escape the repository path', (t) => {
  const root = temporary(t);
  const outside = temporary(t);
  writeFileSync(join(outside, 'v1.0.1.md'), '# Outside');
  symlinkSync(outside, join(root, 'changelog'), 'junction');
  assert.throws(() => validateChangelog('v1.0.1', root), /inside changelog/);
});

test('staging preserves original bytes including BOM and CRLF', (t) => {
  const root = temporary(t);
  mkdirSync(join(root, 'changelog'));
  const bytes = Buffer.from('\uFEFF# v1.0.1\r\n\r\n修正發版記事。\r\n');
  writeFileSync(join(root, 'changelog/v1.0.1.md'), bytes);
  assert.deepEqual(validateChangelog('v1.0.1', root), bytes);
  execFileSync(process.execPath, [resolve('.github/scripts/release-changelog.mjs'), '--stage'], {
    cwd: root, env: { ...process.env, TAG_NAME: 'v1.0.1' },
  });
  assert.deepEqual(readFileSync(join(root, 'dist/changelog.md')), bytes);
});
