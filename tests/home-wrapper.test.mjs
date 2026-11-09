import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

const source = await fs.readFile(new URL('../tv/home.sh', import.meta.url), 'utf8');

// tv/home.sh in a temporary root: the copied state directory with a recording luma-helper,
// the cold-boot hook as a real symlink, and stubs for the root-only id/stat checks.
async function fixture({ helper = true, relative = false, helperExit = 0 } = {}) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'luma-home-wrapper-'));
  const state = path.join(root, 'state');
  const bin = path.join(root, 'bin');
  const hooks = path.join(root, 'init.d');
  const calls = path.join(root, 'calls');
  const sleeps = path.join(root, 'sleeps');
  await Promise.all([state, bin, hooks].map((dir) => fs.mkdir(dir)));
  const line = 'STATE=/var/lib/luma-home\n';
  assert.ok(source.includes(line));
  await fs.writeFile(path.join(state, 'home.sh'), source.replace(line, `STATE='${state}'\n`), {
    mode: 0o755,
  });
  if (helper) {
    await fs.writeFile(
      path.join(state, 'luma-helper'),
      `#!/bin/sh\nfor a; do printf '[%s]' "$a"; done >>'${calls}'\necho >>'${calls}'\nexit ${helperExit}\n`,
      { mode: 0o755 },
    );
  }
  for (const [name, body] of [
    ['id', 'echo 0'],
    ['stat', 'echo 0'],
    ['sleep', `echo "$@" >>'${sleeps}'`],
  ]) {
    await fs.writeFile(path.join(bin, name), `#!/bin/sh\n${body}\n`, { mode: 0o755 });
  }
  const hook = path.join(hooks, '64-luma-home');
  await fs.symlink(relative ? '../state/home.sh' : path.join(state, 'home.sh'), hook);
  const run = (...args) =>
    spawnSync('/bin/sh', [hook, ...args], {
      encoding: 'utf8',
      env: { ...process.env, PATH: `${bin}:${process.env.PATH}` },
      timeout: 10000,
    });
  const lines = async (file) =>
    (await fs.readFile(file, 'utf8').catch(() => '')).split('\n').filter(Boolean);
  const recorded = async () => {
    // Long enough for real retry sleeps where the shell runs sleep as a builtin (busybox ash).
    for (let i = 0; i < 200; i++) {
      const text = await fs.readFile(calls, 'utf8').catch(() => '');
      if (text) return text.trim().split('\n');
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    return [];
  };
  // The detached boot subshell's log, once it has written its final line.
  const bootLog = async () => {
    let log = '';
    for (let i = 0; i < 200 && !log.includes('apply ended'); i++) {
      await new Promise((resolve) => setTimeout(resolve, 50));
      log = await fs.readFile(path.join(state, 'boot.log'), 'utf8').catch(() => '');
    }
    return log;
  };
  return {
    root,
    state,
    run,
    recorded,
    bootLog,
    calls: () => lines(calls),
    sleeps: () => lines(sleeps),
  };
}

for (const relative of [false, true]) {
  test(`the ${relative ? 'relative' : 'absolute'} boot-hook symlink finds the copied helper`, async () => {
    const tv = await fixture({ relative });
    try {
      const status = tv.run('status');
      assert.equal(status.status, 0, status.stderr);
      assert.deepEqual(await tv.recorded(), [`[home][status][${tv.state}][]`]);
    } finally {
      await fs.rm(tv.root, { recursive: true, force: true });
    }
  });
}

test('a cold boot through the hook symlink schedules the copied apply', async () => {
  const tv = await fixture();
  try {
    const boot = tv.run();
    assert.equal(boot.status, 0, boot.stderr);
    assert.deepEqual(await tv.recorded(), [`[home][apply][${tv.state}][]`]);
  } finally {
    await fs.rm(tv.root, { recursive: true, force: true });
  }
});

test('a cold boot applies at once and logs when the hook ran and apply started and ended', async () => {
  const tv = await fixture();
  try {
    assert.equal(tv.run().status, 0);
    const log = await tv.bootLog();
    assert.match(log, /Luma Home: boot hook at [\d.]* s\n/);
    assert.match(log, /Luma Home: apply started at [\d.]* s \(attempt 1\)\n/);
    assert.match(log, /Luma Home: apply ended at [\d.]* s \(exit 0\)\n/);
    assert.deepEqual(await tv.calls(), [`[home][apply][${tv.state}][]`]);
    assert.deepEqual(await tv.sleeps(), []);
  } finally {
    await fs.rm(tv.root, { recursive: true, force: true });
  }
});

test('a failed boot apply is tried three times, two seconds apart', async () => {
  const tv = await fixture({ helperExit: 1 });
  try {
    assert.equal(tv.run().status, 0);
    const log = await tv.bootLog();
    assert.match(log, /apply started at [\d.]* s \(attempt 3\)\n/);
    assert.match(log, /Luma Home: apply ended at [\d.]* s \(exit 1\)\n/);
    assert.deepEqual(await tv.calls(), Array(3).fill(`[home][apply][${tv.state}][]`));
    assert.deepEqual(await tv.sleeps(), ['2', '2']);
  } finally {
    await fs.rm(tv.root, { recursive: true, force: true });
  }
});

test('a fallback runs the copied helper', async () => {
  const tv = await fixture();
  try {
    const fallback = tv.run('fallback');
    assert.equal(fallback.status, 0, fallback.stderr);
    assert.deepEqual(await tv.recorded(), [`[home][fallback][${tv.state}][]`]);
  } finally {
    await fs.rm(tv.root, { recursive: true, force: true });
  }
});

test('a missing helper is reported before any change', async () => {
  const tv = await fixture({ helper: false });
  try {
    const status = tv.run('status');
    assert.equal(status.status, 1);
    assert.match(status.stderr, /native Luma TV helper is missing/);
  } finally {
    await fs.rm(tv.root, { recursive: true, force: true });
  }
});
