import { expect, test } from '@playwright/test';

/**
 * Card #473 (a port of v-note #299) — the SQL console's *database* boundary.
 *
 * This stack has a Traefik edge but no Authentik, and does not route the
 * console through it, so nothing here can say anything
 * about who is allowed to reach the console; that is
 * scripts/smoke-sql-console.sh, which runs post-deploy against the real thing.
 *
 * What these tests cover is the boundary v-note's spike (#299) found was load-bearing.
 * pgweb advertises a `--readonly` mode, but it is a keyword filter over the
 * submitted text plus a read-only transaction — neither has anything to say
 * about a `SELECT`. Pointed at the application's own database user, which the
 * postgres image creates as a superuser, that "read-only" console returned the
 * contents of /etc/passwd via `pg_read_file`. So the privilege separation has
 * to come from Postgres, and this is that finding turned into a regression
 * test: if anyone ever reconnects the console as `bored`, these fail.
 */

const CONSOLE_URL = process.env.SQL_CONSOLE_URL;
const AUTH_USER = process.env.SQL_CONSOLE_AUTH_USER;
const AUTH_PASS = process.env.SQL_CONSOLE_AUTH_PASS;

/**
 * Loud rather than skipped, deliberately.
 *
 * `e2e/docker-compose.test.yml` runs `sqltool` unconditionally and `playwright`
 * hard-depends on it being healthy, so these variables are never legitimately
 * unset in the stack this suite runs against. A `test.skip` would therefore buy
 * nothing, while turning a typo in one compose env var into eight green skips —
 * silently retiring the only automated proof that the `pg_read_file` finding
 * has not regressed.
 */
if (!CONSOLE_URL || !AUTH_USER || !AUTH_PASS) {
  throw new Error(
    'SQL_CONSOLE_URL / SQL_CONSOLE_AUTH_USER / SQL_CONSOLE_AUTH_PASS are unset — ' +
      'the console is part of e2e/docker-compose.test.yml, so this is a broken stack, not an opt-out',
  );
}

/**
 * In a real deployment Traefik adds this header after Authentik has approved
 * the request, so the operator never sees a password prompt. Here the test
 * plays Traefik's part — deliberately, so the console under test is configured
 * exactly as the shipped one is rather than with its auth switched off.
 */
const authHeader = () => ({
  Authorization: `Basic ${Buffer.from(`${AUTH_USER}:${AUTH_PASS}`).toString('base64')}`,
});

/** Runs a statement through the console's own query API, as the UI does. */
async function query(request: import('@playwright/test').APIRequestContext, sql: string) {
  return request.post(`${CONSOLE_URL}/api/query`, {
    headers: authHeader(),
    form: { query: sql },
  });
}

/** pgweb returns rows as `{ columns: [...], rows: [[...]] }`. */
async function rows(request: import('@playwright/test').APIRequestContext, sql: string) {
  const res = await query(request, sql);
  expect(res.status(), `expected ${sql} to succeed, got ${await res.text()}`).toBe(200);
  return (await res.json()).rows as unknown[][];
}

test('the console is not reachable without the basic-auth backstop', async ({ request }) => {
  // The backstop exists so the console is closed at the application layer even
  // if the Traefik middleware that normally gates it is ever missing.
  const res = await request.get(`${CONSOLE_URL}/api/info`);
  expect(res.status()).toBe(401);
});

test('the console connects as a non-superuser role', async ({ request }) => {
  const [[user, isSuper]] = await rows(
    request,
    'select current_user, (select usesuper from pg_user where usename = current_user)',
  );
  expect(user).toBe('bored_pgweb');
  // The whole point. A superuser here re-opens arbitrary host-file read.
  expect(isSuper).toBe(false);
});

test('reads work', async ({ request }) => {
  const [[count]] = await rows(request, 'select count(*) from boards');
  expect(Number(count)).toBeGreaterThanOrEqual(0);
});

test('privileged filesystem access is denied by Postgres, not by pgweb', async ({ request }) => {
  const res = await query(request, "select pg_read_file('/etc/passwd', 0, 20)");
  expect(res.status()).not.toBe(200);
  // Asserting *how* it fails matters: pgweb's keyword filter rejecting this
  // would be a text match that a rephrasing defeats, whereas "permission
  // denied" is the database refusing the role.
  expect((await res.text()).toLowerCase()).toContain('permission denied');
});

test('writes are rejected', async ({ request }) => {
  for (const sql of [
    "insert into boards (id, name) values ('e2e-console', 'nope')",
    'create table e2e_console_should_not_exist (i int)',
    'drop table if exists boards',
  ]) {
    const res = await query(request, sql);
    const body = (await res.text()).toLowerCase();
    expect(res.status(), `expected ${sql} to be rejected`).not.toBe(200);
    // These statements are all valid against the real schema, so a rejection
    // has to be about permission or read-only mode. Without this, a typo'd
    // table name would pass the test for entirely the wrong reason.
    expect(body, `${sql} was rejected, but not because it is a write`).not.toContain(
      'does not exist',
    );
  }

  // And nothing got through.
  const [[leaked]] = await rows(request, "select count(*) from boards where id = 'e2e-console'");
  expect(Number(leaked)).toBe(0);
});

test('the session is read-only and cannot be talked out of it', async ({ request }) => {
  const [[readOnly]] = await rows(request, 'show transaction_read_only');
  expect(readOnly).toBe('on');

  // Postgres refuses to flip this mid-session, which is what makes it a
  // boundary rather than a default.
  const res = await query(request, "select set_config('transaction_read_only', 'off', false)");
  expect(res.status()).not.toBe(200);
});

test('the connection cannot be repointed from the browser', async ({ request }) => {
  // `--lock-session` is what stops the console being aimed at another role or
  // database — including back at the superuser — from the UI.
  const connect = await request.post(`${CONSOLE_URL}/api/connect`, {
    headers: authHeader(),
    form: { url: 'postgres://bored:bored-e2e@db:5432/bored?sslmode=disable' },
  });
  expect(connect.status()).toBe(400);
  expect(await connect.text()).toContain('Session is locked');

  const disconnect = await request.post(`${CONSOLE_URL}/api/disconnect`, {
    headers: authHeader(),
  });
  expect(disconnect.status()).toBe(400);

  // And it really is still the read-only role afterwards.
  const [[user]] = await rows(request, 'select current_user');
  expect(user).toBe('bored_pgweb');
});

test('tables created after the grants are readable without a new grant', async ({ request }) => {
  // Proves ALTER DEFAULT PRIVILEGES took, so keeping this role working is not a
  // chore every future migration has to remember.
  //
  // e2e_default_privileges_probe is created by sqltool-provision *after* the
  // migration ran its GRANTs, which is the only way to test this: any table
  // that already existed is covered by the blanket GRANT ... ON ALL TABLES and
  // would pass whether default privileges work or not.
  const [[count]] = await rows(request, 'select count(*) from e2e_default_privileges_probe');
  expect(Number(count)).toBe(0);
});
