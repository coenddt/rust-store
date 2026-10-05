const test = require('node:test');
const assert = require('node:assert');
const native = require('../index.js');

test('resource pure fns parity', () => {
  assert.strictEqual(native.resourceContentPath('ab12cd'), 'objects/ab/ab12cd');
  assert.strictEqual(native.resourceIsExternalUrl('https://x'), true);
  assert.strictEqual(native.resourceIsExternalUrl('ab12cd'), false);
  assert.strictEqual(
    native.resourceComposeUrl('ab12cd', { baseUrl: 'https://cdn', pathTemplate: '/{contentPath}' }),
    'https://cdn/objects/ab/ab12cd',
  );
});
