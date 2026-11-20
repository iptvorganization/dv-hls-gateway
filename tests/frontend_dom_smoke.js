// Local smoke test: panel metadata is rendered as text, never as markup.
// Run from anywhere with: node /path/to/tests/frontend_dom_smoke.js
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

class Element {
  constructor(tag = 'div') {
    this.tagName = tag;
    this.children = [];
    this.dataset = {};
    this.style = {};
    this.classList = { add() {}, remove() {}, toggle() {} };
    this.textContent = '';
  }
  set innerHTML(_) { throw new Error('Dynamic HTML is not allowed in panel rendering'); }
  append(...items) { this.children.push(...items); }
  appendChild(item) { this.children.push(item); }
  replaceChildren(...items) { this.children = items; }
  addEventListener() {}
}

const elements = new Map();
const document = {
  getElementById(id) {
    if (!elements.has(id)) elements.set(id, new Element());
    return elements.get(id);
  },
  createElement(tag) { return new Element(tag); },
};
const html = fs.readFileSync(path.join(__dirname, '../src/frontend/index.html'), 'utf8');
const js = html.match(/<script>([\s\S]*?)<\/script>/)[1].split('(async function init()')[0];
// Include angle brackets and quote characters typical of upstream labels.
const rawLabel = "Director's cut <Part 1> & extras";
const task = {
  id: 'local-task', name: rawLabel, state: 'running', run_mode: 'always',
  is_dynamic: false, source_kind: 'hls', stage: rawLabel, resolution: '128x72',
  codecs: rawLabel, segments_done: 1, total_segments: 1, segments_available: 1,
  mb_done: 0.5, playlist_url: '/p/local-task', error: rawLabel,
};
const context = vm.createContext({
  document,
  localStorage: { getItem() { return ''; } },
  location: { origin: 'http://127.0.0.1' },
  fetch: async () => ({ status: 200, json: async () => [task] }),
  task,
  fixture: {
    video: [{ id: 'v0', resolution: '128x72', bandwidth: 10, codecs: rawLabel }],
    audio: [{ id: 'a0', lang: rawLabel, codecs: 'AAC' }],
    subtitles: [{ id: 's0', lang: rawLabel, codecs: 'WEBVTT' }],
  },
});
vm.runInContext(js, context);
vm.runInContext('parsed = fixture; renderTracks(); renderDetail(task);', context);
function innerText(node) {
  return typeof node === 'string' ? node : node.textContent + node.children.map(innerText).join('');
}
(async () => {
  await vm.runInContext('refresh()', context);
  for (const id of ['vtracks', 'atracks', 'stracks', 'taskBody', 'detailBody']) {
    assert.ok(innerText(document.getElementById(id)).includes(rawLabel), id);
  }
  console.log('Panel renders upstream text literally in tracks, task table and details');
})().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
