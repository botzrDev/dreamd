'use strict';
const test = require('node:test');
const assert = require('node:assert');
const server = require('../server.json');
const pkg = require('../package.json');

test('registry name matches package.json mcpName', () => {
  assert.strictEqual(server.name, pkg.mcpName);
});

test('top-level and npm package versions match package.json version', () => {
  assert.strictEqual(server.version, pkg.version);
  assert.strictEqual(server.packages[0].version, pkg.version);
});

test('npm package identifier matches package.json name', () => {
  assert.strictEqual(server.packages[0].registryType, 'npm');
  assert.strictEqual(server.packages[0].identifier, pkg.name);
});

test('repository owner, registry namespace, and transport are locked', () => {
  assert.strictEqual(server.repository.url, 'https://github.com/botzrDev/dreamd');
  assert.strictEqual(server.repository.source, 'github');
  assert.strictEqual(server.name, 'io.github.botzrDev/dreamd');
  assert.strictEqual(server.packages[0].transport.type, 'stdio');
});
