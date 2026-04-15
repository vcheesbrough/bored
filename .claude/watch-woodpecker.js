#!/usr/bin/env node
// Watches Woodpecker CI for the latest pipeline after a git push.
// Outputs JSON with additionalContext summarizing results.
// Set WOODPECKER_TOKEN env var for authenticated log access.

const BASE = 'https://woodpecker.desync.link/api/repos/3';
const POLL_INTERVAL = 8000; // ms
const MAX_WAIT = 600000;    // 10 minutes total
const TOKEN = process.env.WOODPECKER_TOKEN;

function authHeaders() {
  return TOKEN ? { Authorization: `Bearer ${TOKEN}` } : {};
}

async function api(path) {
  const r = await fetch(`${BASE}${path}`, { headers: authHeaders() });
  if (!r.ok) throw new Error(`HTTP ${r.status} for ${path}`);
  const ct = r.headers.get('content-type') || '';
  if (!ct.includes('application/json') && !ct.includes('json')) {
    throw new Error(`Expected JSON but got ${ct} for ${path}`);
  }
  return r.json();
}

async function getLatestPipelineNumber() {
  const pipelines = await api('/pipelines?per_page=5&page=1');
  if (!pipelines || pipelines.length === 0) throw new Error('No pipelines found');
  return pipelines[0].number;
}

async function waitForNewPipeline(knownNumber) {
  const deadline = Date.now() + 90000; // 90s to appear
  while (Date.now() < deadline) {
    await sleep(POLL_INTERVAL);
    try {
      const latest = await getLatestPipelineNumber();
      if (latest > knownNumber) return latest;
    } catch (_) { /* retry */ }
  }
  return await getLatestPipelineNumber();
}

async function pollUntilDone(pipelineNum) {
  const deadline = Date.now() + MAX_WAIT;
  while (Date.now() < deadline) {
    const p = await api(`/pipelines/${pipelineNum}`);
    const status = p.status;
    if (status !== 'pending' && status !== 'running') {
      return p;
    }
    process.stderr.write(`Pipeline #${pipelineNum} status: ${status}\n`);
    await sleep(POLL_INTERVAL);
  }
  throw new Error('Timed out waiting for pipeline');
}

async function getFailedLogs(pipeline) {
  const logs = [];
  const workflows = pipeline.workflows || [];
  for (const wf of workflows) {
    const steps = wf.children || wf.steps || [];
    const failed = steps.filter(s => s.state === 'failure' || s.state === 'error');
    for (const step of failed) {
      try {
        let logEntries;
        try {
          // Try with auth if available
          const r = await fetch(`${BASE}/pipelines/${pipeline.number}/logs/${step.id}`, {
            headers: authHeaders(),
          });
          const ct = r.headers.get('content-type') || '';
          if (ct.includes('json')) {
            logEntries = await r.json();
          } else {
            throw new Error('not json');
          }
        } catch {
          logEntries = null;
        }

        if (logEntries) {
          const text = logEntries
            .map(e => {
              try { return Buffer.from(e.data, 'base64').toString('utf8'); }
              catch { return e.data || ''; }
            })
            .join('')
            .trim();
          logs.push(`=== Step: ${step.name} [${wf.name}] (${step.state}) ===\n${text}`);
        } else {
          logs.push(`=== Step: ${step.name} [${wf.name}] (${step.state}) === [logs unavailable — set WOODPECKER_TOKEN env var]`);
        }
      } catch (err) {
        logs.push(`=== Step: ${step.name} [${wf.name}] (${step.state}) === [error fetching logs: ${err.message}]`);
      }
    }
  }
  return logs;
}

function sleep(ms) {
  return new Promise(r => setTimeout(r, ms));
}

async function main() {
  let context;
  try {
    const before = await getLatestPipelineNumber();
    process.stderr.write(`Latest pipeline before push: #${before}\n`);

    process.stderr.write('Waiting for new pipeline...\n');
    const pipelineNum = await waitForNewPipeline(before);
    process.stderr.write(`Watching pipeline #${pipelineNum}\n`);

    const pipeline = await pollUntilDone(pipelineNum);
    const status = pipeline.status;
    const url = `https://woodpecker.desync.link/repos/3/pipeline/${pipelineNum}`;

    // Summarize step results
    const workflows = pipeline.workflows || [];
    const stepSummary = workflows.flatMap(wf =>
      (wf.children || wf.steps || []).map(s => `  ${wf.name}/${s.name}: ${s.state}`)
    ).join('\n');

    if (status === 'success') {
      context = `Woodpecker pipeline #${pipelineNum} PASSED.\n${url}\nSteps:\n${stepSummary}`;
    } else {
      const logs = await getFailedLogs(pipeline);
      context = `Woodpecker pipeline #${pipelineNum} FAILED (${status}).\n${url}\n\nStep summary:\n${stepSummary}\n\nFailed step logs:\n${logs.join('\n\n')}`;
    }
  } catch (err) {
    context = `Woodpecker watcher error: ${err.message}`;
  }

  const output = {
    hookSpecificOutput: {
      hookEventName: 'PostToolUse',
      additionalContext: context,
    },
  };
  process.stdout.write(JSON.stringify(output) + '\n');
}

main();
