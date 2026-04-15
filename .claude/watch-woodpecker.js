#!/usr/bin/env node
// Watches Woodpecker CI for the latest pipeline after a git push.
// Outputs JSON with additionalContext summarizing results.

const BASE = 'https://woodpecker.desync.link/api/repos/3';
const POLL_INTERVAL = 8000; // ms
const MAX_WAIT = 600000;    // 10 minutes total

async function api(path) {
  const r = await fetch(`${BASE}${path}`);
  if (!r.ok) throw new Error(`HTTP ${r.status} for ${path}`);
  return r.json();
}

async function getLatestPipelineNumber() {
  const pipelines = await api('/pipelines?per_page=5&page=1');
  if (!pipelines || pipelines.length === 0) throw new Error('No pipelines found');
  return pipelines[0].number;
}

async function waitForNewPipeline(knownNumber) {
  const deadline = Date.now() + 60000; // 60s to appear
  while (Date.now() < deadline) {
    await sleep(POLL_INTERVAL);
    const latest = await getLatestPipelineNumber();
    if (latest > knownNumber) return latest;
  }
  // If no new pipeline, return the latest (might be from this push)
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

async function getFailedLogs(pipelineNum) {
  const steps = await api(`/pipelines/${pipelineNum}/steps`);
  const failed = steps.filter(s => s.state === 'failure' || s.state === 'error');
  const logs = [];
  for (const step of failed) {
    try {
      const logEntries = await api(`/pipelines/${pipelineNum}/logs/${step.id}`);
      const text = logEntries
        .map(e => {
          try {
            return Buffer.from(e.data, 'base64').toString('utf8');
          } catch {
            return e.data || '';
          }
        })
        .join('')
        .trim();
      logs.push(`=== Step: ${step.name} (${step.state}) ===\n${text}`);
    } catch (err) {
      logs.push(`=== Step: ${step.name} (${step.state}) === [could not fetch logs: ${err.message}]`);
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
    // Record the pipeline number before we start (to detect new one)
    const before = await getLatestPipelineNumber();
    process.stderr.write(`Latest pipeline before push: #${before}\n`);

    // Wait for a new pipeline to appear
    process.stderr.write('Waiting for new pipeline...\n');
    const pipelineNum = await waitForNewPipeline(before);
    process.stderr.write(`Watching pipeline #${pipelineNum}\n`);

    const pipeline = await pollUntilDone(pipelineNum);
    const status = pipeline.status;
    const url = `https://woodpecker.desync.link/repos/3/pipeline/${pipelineNum}`;

    if (status === 'success') {
      context = `Woodpecker pipeline #${pipelineNum} PASSED. ${url}`;
    } else {
      const logs = await getFailedLogs(pipelineNum);
      context = `Woodpecker pipeline #${pipelineNum} FAILED (${status}).\n${url}\n\nFailed step logs:\n${logs.join('\n\n')}`;
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
