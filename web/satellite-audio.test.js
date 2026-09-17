import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

function controls() {
  const elements = new Map();
  for (const id of ['source', 'toggle', 'mute', 'status']) {
    elements.set(`#satellite-${id}`, {
      dataset: {}, attributes: {}, value: '',
      setAttribute(key, value) { this.attributes[key] = value; },
      replaceChildren(...options) { this.options = options; },
    });
  }
  const requests = [];
  const context = vm.createContext({
    document: { querySelector: selector => elements.get(selector) },
    Option: class { constructor(text, value) { this.text = text; this.value = value; } },
    tracks: new Map([
      ['conference', {id: 'conference', name: 'Conference output', enabled: true}],
      ['a', {id: 'a', name: 'Microphone A', enabled: true}],
      ['b', {id: 'b', name: 'Microphone B', enabled: false}],
    ]),
    virtualId: 'conference',
    fetch: async (url, options) => {
      requests.push({url, payload: JSON.parse(options.body)});
      return {ok: true, json: async () => ({})};
    },
  });
  const source = readFileSync(new URL('./audio.js', import.meta.url), 'utf8');
  vm.runInContext(source.slice(source.indexOf('let satelliteState =')), context);
  const sync = state => {
    context.nextState = state;
    vm.runInContext('satelliteState = nextState; renderSatellite();', context);
  };
  return {elements, requests, sync};
}

test('satellite routing and mute send only dedicated device settings', async () => {
  const {elements, requests, sync} = controls();
  sync({enabled: true, source: 'a', muted: false});
  const source = elements.get('#satellite-source');
  assert.deepEqual(source.options.map(option => option.value), ['', 'a', 'b']);
  source.value = 'b';
  await source.onchange();
  assert.deepEqual(requests[0], {url: '/api/v1/audio/satellite', payload: {enabled: true, source: 'b', muted: false}});
  sync({enabled: true, source: 'b', muted: false});
  await elements.get('#satellite-mute').onclick();
  assert.deepEqual(requests[1].payload, {enabled: true, source: 'b', muted: true});
  assert.equal(elements.get('#satellite-toggle').disabled, false);
});

test('missing input stays selected and can be muted without a fallback', async () => {
  const {elements, requests, sync} = controls();
  sync({enabled: true, source: 'unplugged', muted: false});
  assert.equal(elements.get('#satellite-source').value, 'unplugged');
  assert.match(elements.get('#satellite-status').textContent, /Silent/);
  await elements.get('#satellite-mute').onclick();
  assert.equal(requests[0].payload.source, 'unplugged');
  assert.equal(requests[0].payload.muted, true);
  sync({enabled: true, source: 'unplugged', muted: true});
  assert.match(elements.get('#satellite-status').textContent, /Muted/);
});
