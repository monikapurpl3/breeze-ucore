// The μCore deployer's page. It shows what the helper on this computer does,
// and asks it to do things; it talks to nothing else (its Content-Security-
// Policy allows only the helper). Served by the helper itself, or by aspic
// with the helper's port and token in the link the helper opened.
'use strict';

const $ = (id) => document.getElementById(id);
const el = (tag, text, cls) => {
  const e = document.createElement(tag);
  if (text !== undefined) e.textContent = text;
  if (cls) e.className = cls;
  return e;
};

// --- The helper, and the key it handed this page --------------------------

const session = (() => {
  const hash = new URLSearchParams(location.hash.slice(1));
  let token = hash.get('t');
  let port = hash.get('port');
  try {
    if (token) sessionStorage.setItem('ucore-deploy-token', token);
    else token = sessionStorage.getItem('ucore-deploy-token');
    if (port) sessionStorage.setItem('ucore-deploy-port', port);
    else port = sessionStorage.getItem('ucore-deploy-port');
  } catch (_) { /* no storage: the link has to be used again on reload */ }
  // The key does not stay in the address bar, or in the history.
  if (location.hash) history.replaceState(null, '', location.pathname + location.search);
  const local = location.protocol === 'http:' && (location.hostname === '127.0.0.1' || location.hostname === 'localhost');
  const base = local ? '' : 'http://127.0.0.1:' + (Number(port) || 47820);
  return { token, base, local };
})();

// The helper stopped, or this link is from an earlier run of it (every run
// has its own key). Nothing on the page can work after either: say so once,
// show how to start it, and stop asking. Before this, a page left open kept
// polling a helper that was gone, once a second, while its pill still said
// the helper was there.
let started = false;
let lost = false;
let connectionsTimer = null;
function lose(why, unreached) {
  if (lost) return;
  lost = true;
  clearInterval(connectionsTimer);
  const pill = $('helper');
  pill.textContent = why;
  pill.classList.remove('ok');
  pill.classList.add('lost');
  // A page from aspic reaches the helper only if the browser lets it; once it
  // has, that is not what went wrong.
  if (unreached && !started && !session.local) $('no-helper-permission').hidden = false;
  $('no-helper').hidden = false;
}

// A page from aspic reaches the helper on 127.0.0.1 only with the browser's
// leave: Chrome asks the person first (Local Network Access; the permission
// is "loopback-network", earlier "local-network-access"), holds the request
// until they answer, and fails it at once if they said no. Returns 'granted',
// 'prompt' or 'denied', or '' where the browser has no such permission.
async function reachPermission() {
  if (session.local) return 'granted';
  for (const name of ['loopback-network', 'local-network-access']) {
    try {
      return (await navigator.permissions.query({ name })).state;
    } catch (_) { /* not this browser's name for it */ }
  }
  return '';
}

async function api(method, path, body) {
  if (lost) throw new Error('The helper is not answering.');
  const init = { method, headers: { Authorization: 'Bearer ' + session.token } };
  if (body !== undefined) {
    init.headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(body);
  }
  let r;
  try {
    r = await fetch(session.base + '/api/' + path, init);
  } catch (_) {
    // Before the first answer, the start below says why (it may be the
    // browser's permission, not the helper).
    if (started) lose('The helper stopped', true);
    throw new Error('The helper is not answering.');
  }
  if (r.status === 401) {
    lose('This link is from an earlier run of the helper');
    throw new Error('This link is from an earlier run of the helper.');
  }
  const data = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(data.error || ('the helper answered ' + r.status));
  return data;
}

// --- What the helper has connected to --------------------------------------

async function refreshConnections() {
  try {
    const list = await api('GET', 'connections');
    $('connections-count').textContent = list.length;
    const body = $('connections');
    body.replaceChildren(...list.slice().reverse().map((c) => {
      const tr = el('tr');
      tr.append(el('td', new Date(c.at * 1000).toLocaleTimeString()), el('td', c.host), el('td', c.why));
      tr.className = 'kind-' + c.kind;
      return tr;
    }));
  } catch (_) { /* shown by the helper pill */ }
}

// --- Jobs: long work on the helper, its lines as they come -----------------

function follow(job, pre, onEnd) {
  pre.hidden = false;
  pre.textContent = '';
  let since = 0;
  const tick = async () => {
    let s;
    try {
      s = await api('GET', 'jobs/' + job + '?since=' + since);
    } catch (e) {
      pre.textContent += '\n' + e.message;
      return;
    }
    if (s.lines.length) {
      pre.textContent += (pre.textContent ? '\n' : '') + s.lines.join('\n');
      since = s.from + s.lines.length;
      pre.scrollTop = pre.scrollHeight;
    }
    if (s.state === 'running') {
      setTimeout(tick, 700);
    } else {
      pre.textContent += '\n' + (s.state === 'done' ? '✓ Done.' : '✗ ' + s.error);
      pre.scrollTop = pre.scrollHeight;
      refreshConnections();
      if (onEnd) onEnd(s);
    }
  };
  tick();
}

// --- The lists ---------------------------------------------------------------

let boards = [];
let zones = [];
let current = null;

function boardOptions(select, chosenId) {
  select.replaceChildren(...boards.map((b) => {
    const o = el('option', b.name);
    o.value = b.id;
    return o;
  }));
  const custom = el('option', 'Your own board…');
  custom.value = 'custom';
  select.append(custom);
  select.value = chosenId || boards[0]?.id || 'custom';
}

async function showList() {
  current = null;
  $('deployment').hidden = true;
  $('list').hidden = false;
  const list = await api('GET', 'deployments');
  $('deployments').replaceChildren(...list.map((d) => {
    const li = el('li');
    const b = el('button', d.name, 'link');
    b.type = 'button';
    b.addEventListener('click', () => open(d.id));
    li.append(b, el('span', ' · ' + d.board + ' · ' + d.units + (d.units === 1 ? ' unit' : ' units') + (d.running ? ' · running' : ''), 'hint'));
    return li;
  }));
  if (!list.length) $('deployments').append(el('li', 'None yet.', 'hint'));
}

$('new').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  $('new-error').textContent = '';
  const id = $('new-board').value;
  const board = boards.find((b) => b.id === id);
  if (!board) {
    $('new-error').textContent = 'Pick a board first; your own board can be set up once the deployment exists.';
    return;
  }
  try {
    const d = await api('POST', 'deployments', { name: $('new-name').value, board });
    $('new-name').value = '';
    open(d.id);
  } catch (e) {
    $('new-error').textContent = e.message;
  }
});

$('back').addEventListener('click', () => showList());

// --- One deployment ------------------------------------------------------------

async function open(id) {
  current = await api('GET', 'deployments/' + id);
  $('list').hidden = true;
  $('deployment').hidden = false;
  render();
  watchSerial();
}

function render() {
  const d = current;
  $('d-title').textContent = d.name;
  // The board.
  boardOptions($('board-pick'), d.board.id === 'custom' ? 'custom' : d.board.id);
  fillCustom(d.board);
  showCustom();
  $('board-hint').textContent = d.board.network === 'qemu'
    ? 'Runs in Espressif\'s QEMU on this computer (' + (d.board.qemu ? d.board.qemu.program : '') + '), which you install yourself; the deployer looks for it on your PATH.'
    : 'A real board, written to over USB.';
  // The units.
  $('units').replaceChildren(...d.units.map(unitRow));
  $('no-units').hidden = d.units.length > 0;
  // The environment.
  const qemu = d.board.network === 'qemu';
  $('wifi-fields').hidden = qemu;
  $('wifi-qemu').hidden = !qemu;
  $('e-ssid').value = d.env.wifi_ssid || '';
  $('e-pass').value = '';
  $('e-pass').placeholder = d.env.has_wifi_password ? '(kept; type to change it)' : '';
  $('e-ntp').value = d.env.ntp;
  $('e-host').value = d.env.hostname;
  fillZones(d.zone || guessZone(d.env.tz));
  // Running.
  $('run').hidden = !qemu;
  $('stop').hidden = !qemu;
  $('run').disabled = !d.built || d.running;
  $('stop').disabled = !d.running;
  $('configure').disabled = !d.built;
  $('running').hidden = !(qemu && d.running);
  $('address').textContent = 'http://127.0.0.1:' + d.qemu_port;
}

function unitRow(u) {
  const tr = el('tr');
  const name = el('input');
  name.value = u.name;
  // Its address and port, so each field has a name of its own: several
  // units can share an address (an emulated household does).
  name.setAttribute('aria-label', 'Name of the unit at ' + u.ip + ':' + u.port + ' (id ' + u.id + ')');
  name.maxLength = 64;
  name.addEventListener('change', async () => {
    try {
      current = await api('PATCH', 'deployments/' + current.id + '/units/' + u.id, { name: name.value });
      render();
    } catch (e) { alert(e.message); }
  });
  const remove = el('button', 'Remove');
  remove.type = 'button';
  remove.addEventListener('click', async () => {
    if (!confirm('Remove ' + u.name + ' from this deployment?')) return;
    current = await api('DELETE', 'deployments/' + current.id + '/units/' + u.id);
    render();
  });
  const nameCell = el('td');
  nameCell.append(name);
  const actions = el('td');
  actions.append(remove);
  tr.append(nameCell, el('td', u.ip + ':' + u.port), el('td', u.id), el('td', u.has_credentials ? '✓ checked' : '✗ none', u.has_credentials ? 'ok' : 'bad'), actions);
  return tr;
}

// The board form.
function fillCustom(b) {
  $('c-name').value = b.name || '';
  $('c-chip').value = b.chip || 'esp32s3';
  $('c-network').value = b.network || 'wifi';
  $('c-flash').value = b.flash_size || '4mb';
  $('c-qprogram').value = b.qemu ? b.qemu.program : '';
  $('c-qmachine').value = b.qemu ? b.qemu.machine : '';
}
function showCustom() {
  $('custom').hidden = $('board-pick').value !== 'custom';
}
$('board-pick').addEventListener('change', showCustom);
$('board').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  $('board-error').textContent = '';
  const pick = $('board-pick').value;
  let board = boards.find((b) => b.id === pick);
  if (!board) {
    const qemu = $('c-network').value === 'qemu';
    board = {
      id: 'custom', name: $('c-name').value, chip: $('c-chip').value, network: $('c-network').value, flash_size: $('c-flash').value,
      qemu: qemu ? { program: $('c-qprogram').value, machine: $('c-qmachine').value } : null,
    };
  }
  try {
    current = await api('PUT', 'deployments/' + current.id + '/board', board);
    render();
  } catch (e) {
    $('board-error').textContent = e.message;
  }
});

// Time zones: by name, as people know them; the rule is what μCore gets.
function guessZone(rule) {
  let here = '';
  try { here = Intl.DateTimeFormat().resolvedOptions().timeZone || ''; } catch (_) { /* none */ }
  const z = zones.find((z) => z.name === here && (!rule || z.rule === rule));
  return z ? z.name : (zones.find((z) => z.rule === rule) || {}).name || here;
}
function fillZones(chosen) {
  const select = $('e-zone');
  if (!select.options.length) {
    select.append(...zones.map((z) => {
      const o = el('option', z.name.replace(/_/g, ' '));
      o.value = z.name;
      return o;
    }));
  }
  if (chosen) select.value = chosen;
}
$('env').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  $('env-error').textContent = '';
  const zone = zones.find((z) => z.name === $('e-zone').value);
  try {
    current = await api('PUT', 'deployments/' + current.id + '/env', {
      wifi_ssid: $('e-ssid').value,
      wifi_password: $('e-pass').value === '' ? null : $('e-pass').value,
      tz: zone ? zone.rule : 'UTC0',
      zone: zone ? zone.name : '',
      ntp: $('e-ntp').value,
      hostname: $('e-host').value,
    });
    render();
  } catch (e) {
    $('env-error').textContent = e.message;
  }
});

// Units from a config.json: read here, handed to the helper.
$('import-file').addEventListener('change', async () => {
  const file = $('import-file').files[0];
  if (!file) return;
  const out = $('import-result');
  try {
    const r = await api('POST', 'deployments/' + current.id + '/units/import', { text: await file.text() });
    out.textContent = r.added + ' added, ' + r.updated + ' updated.';
    current = await api('GET', 'deployments/' + current.id);
    render();
  } catch (e) {
    out.textContent = e.message;
  }
  $('import-file').value = '';
});

// Units from Midea's cloud.
$('f-look').addEventListener('change', () => {
  $('f-at-label').hidden = $('f-look').value === 'around';
});
$('fetch').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const req = {
    account: $('f-account').value, password: $('f-password').value, app: $('f-app').value,
    look: $('f-look').value, at: $('f-at').value, again: $('f-again').checked,
  };
  // The password leaves this page once, for the helper, and is cleared here.
  $('f-password').value = '';
  try {
    const { job } = await api('POST', 'deployments/' + current.id + '/units/fetch', req);
    follow(job, $('fetch-log'), async () => {
      current = await api('GET', 'deployments/' + current.id);
      render();
    });
  } catch (e) {
    $('fetch-log').hidden = false;
    $('fetch-log').textContent = e.message;
  }
});

// Building, configuring, running.
async function startJob(path) {
  try {
    const { job } = await api('POST', 'deployments/' + current.id + '/' + path);
    follow(job, $('build-log'), async () => {
      current = await api('GET', 'deployments/' + current.id);
      render();
    });
  } catch (e) {
    $('build-log').hidden = false;
    $('build-log').textContent = e.message;
  }
}
$('build').addEventListener('click', () => startJob('build'));
$('configure').addEventListener('click', () => startJob('configure'));
$('run').addEventListener('click', async () => {
  try {
    current = await api('POST', 'deployments/' + current.id + '/run');
    render();
  } catch (e) {
    $('build-log').hidden = false;
    $('build-log').textContent = e.message;
  }
});
$('stop').addEventListener('click', async () => {
  current = await api('POST', 'deployments/' + current.id + '/stop');
  render();
});
$('show-key').addEventListener('click', async () => {
  const { api_key } = await api('GET', 'deployments/' + current.id + '/apikey');
  $('api-key').textContent = api_key;
  $('api-key').hidden = false;
});
$('forget').addEventListener('click', async () => {
  if (!confirm('Forget "' + current.name + '"? Its tokens, Wi-Fi password, API key and flash image are deleted from this computer.')) return;
  await api('DELETE', 'deployments/' + current.id);
  showList();
});

// The console of a running QEMU board.
let serialSince = 0;
let serialFor = null;
function watchSerial() {
  serialFor = current.id;
  serialSince = 0;
  $('serial').textContent = '';
  const tick = async () => {
    if (lost || !current || current.id !== serialFor) return;
    try {
      const s = await api('GET', 'deployments/' + current.id + '/serial?since=' + serialSince);
      if (s.length < serialSince) $('serial').textContent = '';
      if (s.text) {
        $('serial').textContent += s.text;
        $('serial').scrollTop = $('serial').scrollHeight;
      }
      serialSince = s.length;
      if (s.running !== current.running) {
        current = await api('GET', 'deployments/' + current.id);
        render();
      }
    } catch (_) { /* lose() has said so, if that is what it was */ }
    if (!lost) setTimeout(tick, 1000);
  };
  tick();
}

// --- Start -------------------------------------------------------------------

(async () => {
  if (!session.token) {
    $('helper').textContent = 'No helper';
    $('no-helper').hidden = false;
    return;
  }
  if (await reachPermission() === 'prompt') {
    $('helper').textContent = 'Your browser will ask if this page may connect to apps on this computer: allow it, to reach the helper';
  }
  try {
    const hello = await api('GET', 'hello');
    started = true;
    $('helper').textContent = 'Helper ' + hello.version + ' on this computer';
    $('helper').classList.add('ok');
    $('data-dir').textContent = hello.data;
    [boards, zones] = await Promise.all([api('GET', 'boards'), api('GET', 'zones')]);
    boardOptions($('new-board'));
    await showList();
    refreshConnections();
    connectionsTimer = setInterval(refreshConnections, 4000);
  } catch (e) {
    const reach = await reachPermission();
    if (reach === 'denied') lose('Your browser does not let this page connect to apps on this computer', true);
    else if (reach === 'prompt') lose('Your browser asked, and got no answer: reload the page to be asked again', true);
    else lose('The helper did not answer', !session.local);
  }
})();
