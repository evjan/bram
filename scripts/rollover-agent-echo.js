#!/usr/bin/env node

const fs = require("fs");
const path = require("path");

const repoRoot = path.resolve(__dirname, "..");
const rightRoot = path.join(repoRoot, "app", "right");
const livePath = path.join(rightRoot, "live", "AgentEcho.md");
const sessionsDir = path.join(rightRoot, "sessions");
const catalogPath = path.join(sessionsDir, "catalog.json");

const LIVE_TEMPLATE = `# Session transcript

_This is the raw running transcript for the current session, formatted in markdown rather than summarized as cards._
`;

function pad2(value) {
  return String(value).padStart(2, "0");
}

function getTimestampParts(now = new Date()) {
  const year = now.getFullYear();
  const month = pad2(now.getMonth() + 1);
  const day = pad2(now.getDate());
  const hours = pad2(now.getHours());
  const minutes = pad2(now.getMinutes());
  const seconds = pad2(now.getSeconds());

  return {
    archiveTimestamp: `${year}-${month}-${day}-${hours}-${minutes}-${seconds}`,
    createdAt: `${year}-${month}-${day} ${hours}:${minutes}:${seconds}`,
  };
}

function hasTurns(content) {
  return content.includes("### User") || content.includes("### Agent");
}

function ensureFile(pathname, content) {
  if (!fs.existsSync(pathname)) {
    fs.mkdirSync(path.dirname(pathname), { recursive: true });
    fs.writeFileSync(pathname, content, "utf8");
  }
}

ensureFile(livePath, LIVE_TEMPLATE);
ensureFile(catalogPath, "[]\n");

const liveContent = fs.readFileSync(livePath, "utf8");
if (!hasTurns(liveContent)) {
  if (liveContent.trim() !== LIVE_TEMPLATE.trim()) {
    fs.writeFileSync(livePath, LIVE_TEMPLATE, "utf8");
  }
  process.stdout.write("no-rollover\n");
  process.exit(0);
}

const { archiveTimestamp, createdAt } = getTimestampParts();
const filename = `AgentEcho-${archiveTimestamp}.md`;
const archivePath = path.join(sessionsDir, filename);
const catalog = JSON.parse(fs.readFileSync(catalogPath, "utf8"));

fs.mkdirSync(sessionsDir, { recursive: true });
fs.writeFileSync(archivePath, liveContent, "utf8");
catalog.push({
  filename,
  path: `sessions/${filename}`,
  createdAt,
  note: "Recovered live AgentEcho transcript at session startup.",
});
fs.writeFileSync(catalogPath, `${JSON.stringify(catalog, null, 2)}\n`, "utf8");
fs.writeFileSync(livePath, LIVE_TEMPLATE, "utf8");

process.stdout.write(`${filename}\n`);
