import test from 'node:test';
import assert from 'node:assert/strict';
import { collaborationSummary, messagePreview, responseLatency, graphFitScale } from '../src/dashboard/web/collaboration-view.mjs';

test('history summary counts unanswered work and attention without inventing latency', () => {
  const request = {created_at:'2026-10-13T00:00:00Z', status:'completed'};
  const summary = collaborationSummary([
    {...request, reply:{at:'2026-10-13T00:00:10Z'}},
    {...request, reply:{at:'2026-10-13T00:00:30Z'}},
    {...request, status:'claimed'}, {...request, status:'blocked'},
    {...request, status:'cancelled'}, {...request, reply:{at:'invalid'}},
  ]);
  assert.deepEqual(summary, {total:6,replies:3,pending:1,attention:1,medianLatency:20000});
  assert.equal(responseLatency({...request,reply:{at:'2026-10-12T00:00:00Z'}}), null);
  assert.equal(collaborationSummary([]).medianLatency, null);
});

test('message previews never use retained body text when access is locked', () => {
  assert.equal(messagePreview({body:'private contents'},false),'Unlock access to preview message bodies');
  assert.equal(messagePreview({body:'a\n  b'},true),'a b');
  assert.equal(messagePreview({body:'x'.repeat(500)},true).length,240);
});

test('graph fit considers height as well as width and tolerates a hidden viewport', () => {
  assert.equal(graphFitScale(1000,800,1016,416),.5);
  assert.equal(graphFitScale(1000,800,516,816),.5);
  assert.equal(graphFitScale(1000,800,0,0),1);
});

test('unchanged refresh keeps DOM nodes and clearing invalidates cached private HTML', async () => {
  const { setStableHTML, clearStableHTML } = await import('../src/dashboard/web/collaboration-view.mjs');
  let writes = 0;
  const node = {childNodes: [], set innerHTML(value) {writes++; this.childNodes = value ? [{}] : [];}, replaceChildren() {this.childNodes=[];}};
  setStableHTML(node,'private body');
  const original = node.childNodes[0];
  assert.equal(setStableHTML(node,'private body'), false);
  assert.equal(node.childNodes[0], original);
  assert.equal(writes,1);
  clearStableHTML(node);
  assert.equal(node.childNodes.length,0);
  assert.equal(setStableHTML(node,'private body'),true);
});

test('location labels keep complete pane identifiers and message links omit credentials', async () => {
  const {participantLocation,messageLink} = await import('../src/dashboard/web/collaboration-view.mjs');
  assert.equal(participantLocation({agent_kind:'claude_code',pane:'rmux:%123456'}),'rmux:%123456');
  const link = new URL(messageLink('http://localhost:7878/?token=secret#token=secret&workspace=old',{id:'req_123'}));
  assert.equal(link.search,'');
  assert.equal(new URLSearchParams(link.hash.slice(1)).get('message'),'req_123');
  assert.ok(!link.href.includes('secret'));
});
