'use strict';

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const POLL_MS = 1500;
const STARTUP_GRACE_MS = 4000;
const EXPIRY = [['gui-expiry-never', null], ['gui-expiry-hour', 3600], ['gui-expiry-day', 86400],
  ['gui-expiry-week', 7 * 86400], ['gui-expiry-month', 30 * 86400]];
const DEPLOY_STEPS = ['check', 'download', 'configure', 'firewall', 'start'];
const RELEASES_API = 'https://api.github.com/repos/qateralong/weft/releases/latest';
const UPDATE_EVERY_MS = 6 * 3600 * 1000;

let messages = {};
let languages = [];
let appVersion = '0.0.0';
let settings = { notifications: true, updates: true, language: null };
let update = null;
let status = null;
let problem = null;
let startedAt = Date.now();
let pollTimer = null;

const ICONS = {
  power: '<path d="M12 3v9"/><path d="M6.4 6.6a8 8 0 1 0 11.2 0"/>',
  sun: '<circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/>',
  moon: '<path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z"/>',
  gear: '<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  join: '<path d="M15 3h4a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2h-4"/><path d="M10 17l5-5-5-5"/><path d="M15 12H3"/>',
  chevron: '<path d="M6 9l6 6 6-6"/>',
  next: '<path d="M9 6l6 6-6 6"/>',
  globe: '<circle cx="12" cy="12" r="10"/><path d="M2 12h20"/><path d="M12 2a15 15 0 0 1 0 20 15 15 0 0 1 0-20z"/>',
  cloud: '<path d="M18 10h-1.3A7 7 0 1 0 9 19h9a5 5 0 0 0 0-9z"/>',
  monitor: '<rect x="2" y="3" width="20" height="14" rx="2"/><path d="M8 21h8M12 17v4"/>',
  more: '<circle cx="5" cy="12" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/>',
  link: '<path d="M10 13a5 5 0 0 0 7.5.5l3-3a5 5 0 0 0-7-7l-1.7 1.7"/><path d="M14 11a5 5 0 0 0-7.5-.5l-3 3a5 5 0 0 0 7 7l1.7-1.7"/>',
  pulse: '<path d="M22 12h-4l-3 9L9 3l-3 9H2"/>',
  users: '<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M19 8v6M22 11h-6"/>',
};
const AVATAR_COLORS = ['#d9734e', '#c9a03a', '#5f9e57', '#3e9a91', '#3f7fb8', '#7867c4', '#b95f9d', '#8b7258'];
const MODES = {
  online: { icon: 'globe', title: 'gui-mode-online', hint: 'gui-mode-online-hint' },
  vps: { icon: 'cloud', title: 'gui-mode-vps', hint: 'gui-mode-vps-hint' },
  local: { icon: 'monitor', title: 'gui-mode-local', hint: 'gui-mode-local-hint' },
};

/* Basics */

const t = (id, args = {}) => (messages[id] ?? id).replace(/\{\$(\w+)\}/g, (_, name) => args[name] ?? '');
const nodes = (value) => [value].flat(Infinity).filter((node) => node != null && node !== false);

/** Builds an element; `on*` props become handler properties so in-place updates can swap them. */
function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key === 'class') node.className = value;
    else if (key.startsWith('on') || key in node) node[key] = value;
    else node.setAttribute(key, value);
  }
  for (const child of nodes(children)) node.append(child instanceof Node ? child : String(child));
  return node;
}

function icon(name) {
  const span = document.createElement('span');
  span.className = 'icon-box';
  span.innerHTML = `<svg viewBox="0 0 24 24">${ICONS[name]}</svg>`;
  return span;
}

const HANDLERS = ['onclick', 'onchange', 'oninput', 'onkeydown'];

/** Updates `from` to look like `to` while keeping unchanged nodes, so hover, focus and animations survive. */
function morph(from, to) {
  if (from.nodeType !== to.nodeType || from.nodeName !== to.nodeName) {
    from.replaceWith(to);
    return;
  }
  if (from.nodeType !== Node.ELEMENT_NODE) {
    if (from.nodeValue !== to.nodeValue) from.nodeValue = to.nodeValue;
    return;
  }
  for (const attr of [...from.attributes]) {
    if (!to.hasAttribute(attr.name)) from.removeAttribute(attr.name);
  }
  for (const attr of [...to.attributes]) {
    if (from.getAttribute(attr.name) !== attr.value) from.setAttribute(attr.name, attr.value);
  }
  for (const key of HANDLERS) {
    if (from[key] !== to[key]) from[key] = to[key];
  }
  if ('disabled' in from && from.disabled !== to.disabled) from.disabled = to.disabled;
  if (from !== document.activeElement && 'value' in from && from.nodeName !== 'BUTTON' && from.value !== to.value) {
    from.value = to.value;
  }
  if ('checked' in from && from.checked !== to.checked) from.checked = to.checked;
  morphChildren(from, to);
}

function morphChildren(from, to) {
  const old = [...from.childNodes];
  const fresh = [...to.childNodes];
  fresh.forEach((node, index) => {
    if (index < old.length) morph(old[index], node);
    else from.appendChild(node);
  });
  for (const node of old.slice(fresh.length)) node.remove();
}

function stored(key, fallback) {
  try { return localStorage.getItem(key) ?? fallback; } catch { return fallback; }
}

function store(key, value) {
  try { localStorage.setItem(key, value); } catch { /* storage unavailable */ }
}

function applyTheme(theme) {
  document.documentElement.dataset.theme = theme;
}

function toggleTheme() {
  const theme = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
  store('theme', theme);
  applyTheme(theme);
  render();
}

applyTheme(stored('theme', matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light'));

const request = (command, fields = {}, server = null) => invoke('request', { request: { command, ...fields }, server });

let toastTimer = null;
function toast(text, error = false) {
  const node = document.getElementById('toast');
  node.textContent = text;
  node.className = error ? 'error show' : 'show';
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => node.classList.remove('show'), error ? 5000 : 2500);
}

async function act(command, fields, done, server = null) {
  try {
    const response = await request(command, fields, server);
    if (response.result === 'joined') toast(t('done-join', { name: response.data }));
    else if (response.result === 'pending') toast(t('done-pending', { name: response.data }));
    else if (done) toast(done);
    await poll();
    return response;
  } catch (error) {
    toast(String(error), true);
    return null;
  }
}

async function copy(text) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const area = el('textarea', { value: text });
    document.body.append(area);
    area.select();
    document.execCommand('copy');
    area.remove();
  }
  toast(t('gui-copied'));
}

function remaining(seconds) {
  const minutes = Math.max(1, Math.ceil(seconds / 60));
  const [days, hours, mins] = [Math.floor(minutes / 1440), Math.floor(minutes / 60) % 24, minutes % 60];
  if (days > 0) return t('time-days', { days, hours });
  if (hours > 0) return t('time-hours', { hours, minutes: mins });
  return t('time-minutes', { minutes: mins });
}

function linkText(member) {
  switch (member.link) {
    case 'direct': return t('link-direct');
    case 'relay': return t('link-relay');
    case 'connecting': return t('link-connecting');
    default: return t('link-offline');
  }
}

/* Servers and modes */

/** Connected when any server is, connecting when any tries to. */
function overall(current) {
  const states = (current?.servers ?? []).map((server) => server.connection);
  if (states.includes('connected')) return 'connected';
  return states.includes('connecting') ? 'connecting' : 'disconnected';
}

/** Which of the three kinds a server is. */
function kindOf(server) {
  if (server.hosted) return 'local';
  return server.public ? 'online' : 'vps';
}

/** Servers to show, with the hosted one even before the daemon connects to it. */
function serverList(current) {
  const servers = current?.servers ?? [];
  if (current?.host && !servers.some((server) => server.hosted)) {
    return [{ server: 'local', host: '127.0.0.1', connection: 'connecting', networks: [], hosted: true, public: false }, ...servers];
  }
  return servers;
}

const several = () => serverList(status).length > 1;

function serverName(server) {
  if (server.public) return t('gui-mode-online');
  if (server.hosted) return t('gui-mode-local');
  return server.host;
}

/** Waits until the server `pick` finds is connected and returns it. */
async function connected(pick, timeoutMs = 20000) {
  const until = Date.now() + timeoutMs;
  for (;;) {
    await poll();
    const server = serverList(status).find(pick);
    if (server?.connection === 'connected') return server;
    if (Date.now() > until) throw new Error(t('gui-server-unreachable'));
    await new Promise((resolve) => setTimeout(resolve, 700));
  }
}

/**
 * The server choice for creating or joining a network: servers in use, plus the kinds not added
 * yet. Returns [nodes, resolve], where resolve sets the chosen server up and returns its link.
 */
function serverChooser(onlyNew = false) {
  const servers = status?.servers ?? [];
  const options = onlyNew ? [] : servers.map((server) => [server.server, serverName(server)]);
  if (!servers.some((server) => server.public) && status?.public_link) options.push(['new:online', t('gui-mode-online')]);
  if (!status?.host) options.push(['new:local', t('gui-mode-local')]);
  options.push(['new:vps', t('gui-mode-vps')]);
  const preferred = servers.find((server) => server.connection === 'connected')?.server ?? options[0][0];
  const select = el('select', {}, options.map(([value, label]) => el('option', { value, selected: value === preferred }, label)));
  const hint = el('p', { class: 'muted hint' });
  const [hostLabel, host] = field('gui-vps-host', { placeholder: '203.0.113.10', autocomplete: 'off' });
  const [userLabel, user] = field('gui-vps-user', { value: 'root', autocomplete: 'off' });
  const [passLabel, password] = field('gui-vps-password', { type: 'password' });
  const [portLabel, port] = field('gui-vps-port', { type: 'number', min: 1, max: 65535, value: 22 });
  const vps = el('div', { class: 'vps', hidden: true }, hostLabel, userLabel, passLabel, el('details', {}, el('summary', {}, t('gui-advanced')), portLabel));
  const progress = el('div', { class: 'steps', hidden: true }, DEPLOY_STEPS.map((step) => el('div', { class: 'step', 'data-step': step },
    el('span', { class: 'step-mark' }), t(`gui-deploy-${step}`))));
  const update = () => {
    const mode = select.value.startsWith('new:') ? select.value.slice(4) : null;
    hint.textContent = mode ? t(MODES[mode].hint) : '';
    hint.hidden = !mode;
    vps.hidden = mode !== 'vps';
  };
  select.onchange = update;
  update();

  const deploy = async () => {
    if (!host.value.trim() || !password.value) throw new Error(t('gui-vps-missing'));
    progress.hidden = false;
    for (const node of progress.children) node.className = 'step';
    const unlisten = await listen('deploy-step', (event) => {
      let reached = false;
      for (const node of progress.children) {
        if (node.dataset.step === event.payload) { node.className = 'step active'; reached = true; } else if (!reached) node.className = 'step done';
      }
    });
    try {
      const link = await invoke('deploy_server', {
        host: host.value.trim(), port: Number(port.value) || 22, user: user.value.trim() || 'root', password: password.value,
      });
      for (const node of progress.children) node.className = 'step done';
      return link;
    } catch (error) {
      for (const node of progress.children) if (node.className === 'step active') node.className = 'step failed';
      throw error;
    } finally {
      unlisten();
    }
  };

  const resolve = async () => {
    switch (select.value) {
      case 'new:online':
        await request('up', { link: status.public_link, nickname: null });
        return (await connected((server) => server.public)).server;
      case 'new:local':
        await request('host', { enabled: true, port: null, address: null });
        return (await connected((server) => server.hosted)).server;
      case 'new:vps': {
        const link = await deploy();
        await request('up', { link, nickname: null });
        return (await connected((server) => server.server === link)).server;
      }
      default:
        return select.value;
    }
  };
  return [[el('label', {}, el('span', {}, t(onlyNew ? 'gui-server-kind' : 'gui-server')), select), hint, vps, progress], resolve];
}

/* Dialogs */

/** Opens the dialog; with `back`, the leave button and Escape go to that dialog instead of closing. */
function openDialog(title, build, actions = [], back = null) {
  const dialog = document.getElementById('dialog');
  const body = el('div', { class: 'body' });
  const error = el('div', { class: 'error', hidden: true });
  const fail = (text) => { error.textContent = text; error.hidden = false; };
  const close = () => {
    if (!dialog.open || dialog.classList.contains('closing')) return;
    dialog.classList.add('closing');
    setTimeout(() => { dialog.classList.remove('closing'); dialog.close(); }, 150);
  };
  const render = async () => {
    body.replaceChildren(error, ...nodes(await build({ close, fail, render })));
  };
  const buttons = actions.map(({ label, primary: main, run }) => el('button', {
    class: main ? 'primary' : '',
    onclick: async (event) => {
      const button = event.currentTarget;
      error.hidden = true;
      button.disabled = true;
      try {
        if (await run({ close, fail, body }) !== false) close();
      } catch (e) {
        fail(String(e));
      } finally {
        button.disabled = false;
      }
    },
  }, label));
  const leave = back ?? close;
  const leaveLabel = back ? 'gui-back' : actions.length ? 'gui-cancel' : 'gui-close';
  dialog.replaceChildren(el('h3', {}, title), body,
    el('div', { class: 'actions' }, el('button', { onclick: () => leave() }, t(leaveLabel)), buttons));
  dialog.oncancel = (event) => { event.preventDefault(); leave(); };
  dialog.onkeydown = (event) => {
    if (event.key === 'Enter' && event.target.tagName === 'INPUT' && buttons.length) buttons[buttons.length - 1].click();
  };
  render().then(() => {
    if (!dialog.open) dialog.showModal();
    body.querySelector('input')?.focus();
  });
  return close;
}

const closeDialog = () => document.getElementById('dialog').close();

function field(labelId, props = {}) {
  const input = el('input', props);
  return [el('label', {}, el('span', {}, t(labelId)), input), input];
}

function dangerButton(label, run) {
  let armed = null;
  const button = el('button', { class: 'danger' }, label);
  button.onclick = async () => {
    if (!armed) {
      button.classList.add('armed');
      button.textContent = t('gui-confirm');
      armed = setTimeout(() => { armed = null; button.classList.remove('armed'); button.textContent = label; }, 4000);
      return;
    }
    clearTimeout(armed);
    await run();
  };
  return button;
}

function diagnosticsDialog() {
  let text = '';
  openDialog(t('gui-diagnostics'), async ({ fail, render: refresh }) => {
    try {
      text = await invoke('diagnostics');
    } catch (e) {
      fail(String(e));
    }
    return [
      el('pre', { class: 'mono report' }, text),
      el('div', { class: 'toolbar' },
        el('button', { onclick: () => refresh() }, t('gui-refresh')),
        el('button', { onclick: () => copy(text) }, t('gui-copy')),
        el('button', {
          class: 'primary',
          onclick: async () => {
            try {
              toast(t('done-report-saved', { path: await invoke('save_report') }));
            } catch (e) { fail(String(e)); }
          },
        }, t('gui-save-report'))),
    ];
  });
}

function newer(tag) {
  const parse = (text) => text.replace(/^v/, '').split('.').map((part) => parseInt(part, 10) || 0);
  const [a, b] = [parse(tag), parse(appVersion)];
  for (let i = 0; i < 3; i += 1) {
    if ((a[i] ?? 0) !== (b[i] ?? 0)) return (a[i] ?? 0) > (b[i] ?? 0);
  }
  return false;
}

async function checkUpdates() {
  if (!settings.updates) {
    update = null;
    return;
  }
  let info = null;
  try { info = JSON.parse(stored('update', 'null')); } catch { info = null; }
  if (!info || Date.now() - info.at > UPDATE_EVERY_MS) {
    try {
      const response = await fetch(RELEASES_API, { headers: { Accept: 'application/vnd.github+json' } });
      if (!response.ok) return;
      const release = await response.json();
      info = { tag: release.tag_name, url: release.html_url, at: Date.now() };
      store('update', JSON.stringify(info));
    } catch {
      return;
    }
  }
  const next = info && newer(info.tag) ? info : null;
  if (JSON.stringify(next) !== JSON.stringify(update)) {
    update = next;
    render();
  }
}

function settingToggle(labelId, key) {
  const box = el('input', {
    type: 'checkbox', checked: settings[key],
    onchange: async () => {
      settings = { ...settings, [key]: box.checked };
      try { await invoke('set_settings', { settings }); } catch (e) { toast(String(e), true); }
      if (key === 'updates') { update = null; render(); checkUpdates(); }
    },
  });
  return el('label', { class: 'check' }, box, el('span', {}, t(labelId)));
}

function languagePicker() {
  const select = el('select', {
    onchange: async () => {
      const language = select.value || null;
      try {
        await invoke('set_language', { language });
        settings = { ...settings, language };
        await loadCatalog();
        render();
        settingsDialog();
      } catch (e) { toast(String(e), true); }
    },
  },
  el('option', { value: '', selected: !settings.language }, t('gui-language-system')),
  languages.map(([code, name]) => el('option', { value: code, selected: settings.language === code }, name)));
  return el('label', {}, el('span', {}, t('gui-language')), select);
}

function settingsDialog() {
  const [nickLabel, nickname] = field('gui-nickname', { value: status?.nickname ?? '' });
  openDialog(t('gui-settings'), () => [
    nickLabel,
    languagePicker(),
    el('button', { class: 'menu-item', onclick: serversDialog },
      el('span', { class: 'grow' }, t('gui-servers')),
      el('span', { class: 'muted' }, String(serverList(status).length)),
      icon('next')),
    el('div', { class: 'section' }, t('gui-app')),
    settingToggle('gui-notifications', 'notifications'),
    settingToggle('gui-check-updates', 'updates'),
  ], [{
    label: t('gui-save'), primary: true,
    run: async () => {
      const name = nickname.value.trim();
      if (name && name !== status?.nickname) await request('up', { link: null, nickname: name });
      await poll();
    },
  }]);
}

function serverMenu(server) {
  const local = server.hosted;
  openDialog(serverName(server), ({ fail }) => {
    const item = (label, run) => el('button', { onclick: run }, label);
    const link = local ? status.host?.link : server.server;
    return el('div', { class: 'menu' },
      local && item(t('gui-host-advanced'), hostDialog),
      link && item(t('gui-copy-server-link'), () => { copy(link); serversDialog(); }),
      dangerButton(t(local ? 'gui-host-stop' : 'gui-server-remove'), async () => {
        try {
          await request(local ? 'host' : 'remove', local ? { enabled: false, port: null, address: null } : {}, local ? null : server.server);
          toast(t('done-remove', { server: serverName(server) }));
          await poll();
          serversDialog();
        } catch (e) { fail(String(e)); }
      }));
  }, [], serversDialog);
}

function hostDialog() {
  const host = status?.host;
  if (!host) return;
  const [addressLabel, address] = field('gui-host-address', { value: host.address ?? '', placeholder: t('gui-host-address-auto') });
  const [portLabel, port] = field('gui-host-port', { type: 'number', min: 1, max: 65535, value: host.port });
  openDialog(t('gui-host-advanced'), () => [
    el('p', { class: 'muted' }, t('gui-host-advanced-hint')), addressLabel, portLabel,
  ], [{
    label: t('gui-save'), primary: true,
    run: async () => {
      const value = Number(port.value);
      await request('host', { enabled: true, port: value > 0 ? value : null, address: address.value.trim() });
      await poll();
      serversDialog();
      return false;
    },
  }], serversDialog);
}

function serverRow(server) {
  const mode = kindOf(server);
  const host = server.hosted ? status.host : null;
  const reach = host && t({ public: 'host-reach-public', local: 'host-reach-local', behind: 'host-reach-behind' }[host.reach]);
  return el('div', { class: 'server-row' },
    el('div', { class: 'server-line' },
      icon(MODES[mode].icon),
      el('div', { class: 'grow' },
        el('b', {}, t(MODES[mode].title)),
        el('div', { class: 'muted sub' }, el('span', { class: `dot ${server.connection}` }), t(`state-${server.connection}`)),
        mode !== 'local' && el('div', { class: 'muted info mono' }, server.host)),
      signal(serverLatency(server)),
      el('button', { class: 'icon', title: t('gui-settings'), onclick: () => serverMenu(server) }, icon('more'))),
    host && el('p', { class: 'muted note' }, reach, !(host.reach === 'public' && host.mapped) && `. ${t(host.mapped ? 'host-mapped' : 'host-not-mapped', { port: host.port })}`));
}

function serversDialog() {
  openDialog(t('gui-servers'), () => {
    const servers = serverList(status);
    return [
      el('div', { class: 'list servers' }, servers.length ? servers.map(serverRow) : el('div', { class: 'row empty' }, t('gui-no-servers'))),
      el('button', { class: 'add-server', onclick: addServerDialog }, icon('plus'), t('gui-add-server')),
    ];
  }, [], settingsDialog);
}

function addServerDialog() {
  const [nodes, resolve] = serverChooser(true);
  openDialog(t('gui-add-server'), () => nodes, [{
    label: t('gui-add'), primary: true,
    run: async () => {
      await resolve();
      toast(t('gui-server-added'));
      serversDialog();
      return false;
    },
  }], serversDialog);
}

function createDialog() {
  const [nameLabel, name] = field('gui-network-name', { maxLength: 64 });
  const [passLabel, password] = field('gui-password', { type: 'password' });
  const [repeatLabel, repeat] = field('gui-password-repeat', { type: 'password' });
  const [server, resolve] = serverChooser();
  openDialog(t('gui-create-network'), () => [nameLabel, passLabel, repeatLabel, server], [{
    label: t('gui-create'), primary: true,
    run: async ({ fail }) => {
      if (!name.value.trim()) return fail(t('gui-name-missing')) ?? false;
      if (password.value !== repeat.value) return fail(t('password-mismatch')) ?? false;
      await request('create', { name: name.value.trim(), password: password.value }, await resolve());
      toast(t('done-create', { name: name.value.trim() }));
      await poll();
    },
  }]);
}

function joinDialog() {
  const [targetLabel, target] = field('gui-name-or-link', { placeholder: 'weft://…' });
  const [passLabel, password] = field('gui-password', { type: 'password' });
  const [server, resolve] = serverChooser();
  const byName = el('div', {}, passLabel, server);
  target.oninput = () => { byName.hidden = target.value.trim().toLowerCase().startsWith('weft://'); };
  openDialog(t('gui-join-network'), () => [targetLabel, byName], [{
    label: t('gui-join'), primary: true,
    run: async () => {
      const value = target.value.trim();
      const response = byName.hidden
        ? await request('redeem', { link: value })
        : await request('join', { name: value, password: password.value }, await resolve());
      if (response.result === 'joined') toast(t('done-join', { name: response.data }));
      else if (response.result === 'pending') toast(t('done-pending', { name: response.data }));
      else toast(t('done-join', { name: value }));
      await poll();
    },
  }]);
}

function inviteDetails(invite) {
  const uses = invite.max_uses != null
    ? t('invite-uses', { uses: invite.uses, max: invite.max_uses })
    : t('invite-uses-unlimited', { uses: invite.uses });
  const expires = invite.expires != null
    ? t('invite-expires', { time: remaining(invite.expires - Date.now() / 1000) })
    : t('invite-no-expiry');
  return `${uses} · ${expires} · ${t('invite-by', { nickname: invite.creator })}`;
}

function invitesDialog({ name: network, server }) {
  const [usesLabel, uses] = field('gui-invite-uses', { type: 'number', min: 1 });
  const expiry = el('select', {}, EXPIRY.map(([id, seconds], index) =>
    el('option', { value: seconds ?? '', selected: index === 3 }, t(id))));
  let created = null;
  openDialog(t('gui-invites'), async ({ fail, render: refresh }) => {
    const create = el('button', {
      class: 'primary',
      onclick: async () => {
        try {
          const response = await request('create_invite', {
            network, uses: uses.value ? Number(uses.value) : null,
            expires_in: expiry.value ? Number(expiry.value) : null,
          }, server);
          created = response.data;
          await refresh();
        } catch (e) { fail(String(e)); }
      },
    }, t('gui-create'));
    const result = created && el('div', { class: 'list' }, el('div', { class: 'row' },
      el('span', { class: 'grow mono wrap' }, created.link ?? created.code),
      el('button', { class: 'small', onclick: () => copy(created.link ?? created.code) }, t('gui-copy'))));
    let list;
    try {
      const invites = (await request('invites', { network }, server)).data;
      list = invites.length
        ? invites.map((invite) => el('div', { class: 'row' },
          el('div', { class: 'grow wrap' }, el('div', { class: 'mono' }, invite.code), el('div', { class: 'muted' }, inviteDetails(invite))),
          el('button', { class: 'small', onclick: () => copy(invite.link ?? invite.code) }, t('gui-copy')),
          el('button', {
            class: 'small danger',
            onclick: async () => {
              try { await request('revoke_invite', { code: invite.code }, server); await refresh(); } catch (e) { fail(String(e)); }
            },
          }, t('gui-revoke'))))
        : el('div', { class: 'row empty' }, t('invites-empty', { name: network }));
    } catch (e) {
      fail(String(e));
    }
    return [
      el('div', { class: 'section' }, t('gui-new-invite')),
      usesLabel,
      el('label', {}, el('span', {}, t('gui-invite-expiry')), expiry),
      create, result && el('div', { class: 'spaced' }, result),
      el('div', { class: 'section' }, t('invites-title', { name: network })),
      el('div', { class: 'list' }, list),
    ];
  });
}

function devicesDialog({ name: network, server }, kind) {
  const actions = kind === 'requests'
    ? [['gui-approve', 'approve', 'primary'], ['gui-deny', 'deny', 'danger']]
    : [['gui-unban', 'unban', '']];
  openDialog(t(kind === 'requests' ? 'gui-requests' : 'gui-bans'), async ({ fail, render: refresh }) => {
    let devices = [];
    try {
      devices = (await request(kind, { network }, server)).data;
    } catch (e) {
      fail(String(e));
    }
    if (!devices.length) return el('div', { class: 'list' }, el('div', { class: 'row empty' }, t(`${kind}-empty`, { name: network })));
    return el('div', { class: 'list' }, devices.map((device) => el('div', { class: 'row' },
      el('div', { class: 'grow' }, el('div', {}, device.nickname), el('div', { class: 'muted mono' }, device.address)),
      actions.map(([label, command, style]) => el('button', {
        class: `small ${style}`,
        onclick: async () => {
          try {
            await request(command, { network, member: device.public_key }, server);
            await poll();
            await refresh();
          } catch (e) { fail(String(e)); }
        },
      }, t(label))))));
  });
}

function networkSettingsDialog(network) {
  openDialog(t('gui-network-settings'), ({ fail, close }) => {
    const current = status.servers.find((server) => server.server === network.server)
      ?.networks.find((n) => n.name === network.name) ?? network;
    const owner = current.role === 'owner';
    const configure = (fields) => request('configure', { network: network.name, locked: null, approval: null, password: null, ...fields }, network.server);
    const toggle = (labelId, key) => {
      const box = el('input', {
        type: 'checkbox', checked: current[key],
        onchange: async () => {
          try {
            await configure({ [key]: box.checked });
            await poll();
          } catch (e) { box.checked = !box.checked; fail(String(e)); }
        },
      });
      return el('label', { class: 'check' }, box, el('span', {}, t(labelId)));
    };
    const parts = [toggle('gui-locked', 'locked'), toggle('gui-approval', 'approval')];
    if (owner) {
      const [passLabel, password] = field('gui-new-password', { type: 'password' });
      parts.push(el('div', { class: 'section' }, t('gui-change-password')), passLabel, el('button', {
        onclick: async () => {
          try {
            await configure({ password: password.value });
            password.value = '';
            toast(t('done-password', { name: network.name }));
          } catch (e) { fail(String(e)); }
        },
      }, t('gui-change-password')));
      parts.push(el('div', { class: 'section' }, t('gui-delete-network')), dangerButton(t('gui-delete-network'), async () => {
        try {
          await request('delete', { network: network.name }, network.server);
          toast(t('done-delete', { name: network.name }));
          close();
          await poll();
        } catch (e) { fail(String(e)); }
      }));
    }
    return parts;
  });
}

function memberDialog(network, member) {
  const owner = network.role === 'owner';
  const actions = [['gui-kick', 'kick', 'done-kick'], ['gui-ban', 'ban', 'done-ban']];
  if (owner) actions.push(['gui-promote', 'set_role', 'done-promote', 'admin'], ['gui-demote', 'set_role', 'done-demote', 'member']);
  openDialog(member.nickname, ({ fail, close }) => [
    el('p', { class: 'muted mono' }, member.address),
    el('div', { class: 'toolbar wrap' }, actions.map(([label, command, done, role]) => el('button', {
      class: command === 'set_role' ? '' : 'danger',
      onclick: async () => {
        try {
          const fields = { network: network.name, member: member.address };
          if (role) fields.role = role;
          await request(command, fields, network.server);
          toast(t(done, { name: network.name, member: member.nickname }));
          close();
          await poll();
        } catch (e) { fail(String(e)); }
      },
    }, t(label)))),
  ]);
}

function networkMenu(network) {
  const manager = network.role !== 'member';
  openDialog(network.name, ({ close }) => {
    const item = (label, run, style = '') => el('button', {
      class: style,
      onclick: () => { close(); setTimeout(run, 170); },
    }, label);
    return el('div', { class: 'menu' },
      manager && item(t('gui-invites'), () => invitesDialog(network)),
      manager && item(t('gui-requests'), () => devicesDialog(network, 'requests')),
      manager && item(t('gui-bans'), () => devicesDialog(network, 'bans')),
      manager && item(t('gui-network-settings'), () => networkSettingsDialog(network)),
      item(t('gui-leave'), () => act('leave', { name: network.name }, t('done-leave', { name: network.name }), network.server), 'danger'));
  });
}

/* Rendering */

let seen = new Set();
let fresh = new Set();
const collapsed = new Set(JSON.parse(stored('collapsed', '[]')));

function entering(node, key) {
  node.dataset.key = key;
  fresh.add(key);
  if (!seen.has(key)) node.classList.add('enter');
  return node;
}

function avatar(member) {
  let hash = 0;
  for (const char of member.nickname) hash = (hash * 31 + char.codePointAt(0)) >>> 0;
  const node = el('div', { class: 'avatar' }, [...member.nickname][0] ?? '?', el('span', { class: `dot ${member.link}` }));
  node.style.background = AVATAR_COLORS[hash % AVATAR_COLORS.length];
  return node;
}

function header() {
  const dark = document.documentElement.dataset.theme === 'dark';
  const tools = [
    el('button', { class: 'icon', title: t('gui-diagnostics'), onclick: diagnosticsDialog }, icon('pulse')),
    el('button', { class: 'icon', title: t(dark ? 'gui-theme-light' : 'gui-theme-dark'), onclick: toggleTheme }, icon(dark ? 'sun' : 'moon')),
    el('button', { class: 'icon', title: t('gui-settings'), onclick: settingsDialog }, icon('gear')),
  ];
  if (!serverList(status).length) {
    return el('header', {}, el('div', { class: 'me' }, el('div', { class: 'name' }, 'Weft')), tools);
  }
  const connection = overall(status);
  const connected = connection !== 'disconnected';
  const address = status.servers.find((server) => server.address)?.address;
  return el('header', {},
    el('button', {
      class: `power ${connection}`,
      title: t(connected ? 'gui-disconnect' : 'gui-connect'),
      onclick: () => act(connected ? 'down' : 'up', connected ? {} : { link: null, nickname: null }),
    }, icon('power')),
    el('div', { class: 'me' },
      el('div', { class: 'name' }, status.nickname),
      el('div', { class: 'sub' },
        el('span', { class: `dot ${connection}` }),
        t(`state-${connection}`),
        address && el('span', { class: 'mono address', title: t('gui-copy'), onclick: () => copy(address) }, address))),
    tools);
}

function welcome() {
  const [nickLabel, nickname] = field('gui-nickname', { value: status?.nickname ?? '' });
  nickname.onchange = () => {
    const name = nickname.value.trim();
    if (name && name !== status?.nickname) request('up', { link: null, nickname: name }).catch(() => {});
  };
  return el('div', { class: 'card hero', 'data-key': 'welcome' },
    el('img', { src: 'icon.png', alt: '' }),
    el('h2', {}, t('gui-welcome')),
    el('p', {}, t('gui-welcome-hint')),
    nickLabel,
    el('div', { class: 'toolbar' },
      el('button', { onclick: createDialog }, icon('plus'), t('gui-create-network')),
      el('button', { class: 'primary', onclick: joinDialog }, icon('join'), t('gui-join-network'))));
}

/** Signal bars and the round trip; `estimate` marks a guess. */
function signal(ms, estimate = false) {
  const level = ms == null ? 0 : ms < 60 ? 4 : ms < 120 ? 3 : ms < 250 ? 2 : 1;
  const text = ms == null ? '—' : t('gui-ping-ms', { ms: ms < 1 ? '<1' : `${estimate ? '~' : ''}${ms}` });
  return el('div', { class: 'signal', title: t(estimate ? 'gui-ping-relay' : 'gui-ping') },
    el('span', { class: 'bars' }, [1, 2, 3, 4].map((bar) => el('i', { class: bar <= level ? `on level-${level}` : '' }))),
    el('span', { class: 'ms' }, text));
}

const serverLatency = (server) => (server?.connection === 'connected' ? server.latency_ms ?? null : null);

function toggleNetwork(card, id) {
  if (collapsed.has(id)) collapsed.delete(id); else collapsed.add(id);
  card.classList.toggle('collapsed', collapsed.has(id));
  store('collapsed', JSON.stringify([...collapsed]));
}

function networkCard(network) {
  const manager = network.role !== 'member';
  const tags = [el('span', { class: 'tag role' }, t(`role-${network.role}`))];
  if (network.locked) tags.push(el('span', { class: 'tag' }, t('network-locked')));
  if (network.approval) tags.push(el('span', { class: 'tag' }, t('network-approval')));
  if (several()) tags.push(el('span', { class: 'tag' }, serverName(status.servers.find((server) => server.server === network.server) ?? {})));
  const online = network.members.filter((member) => member.link !== 'offline').length;
  const stop = (run) => (event) => { event.stopPropagation(); run(); };
  const id = `${network.server}/${network.name}`;
  const card = el('div', { class: `card${collapsed.has(id) ? ' collapsed' : ''}` });
  const head = el('div', { class: 'network-head', onclick: (event) => toggleNetwork(event.currentTarget.parentElement, id) },
    el('span', { class: 'chevron' }, icon('chevron')),
    el('div', { class: 'title' },
      el('b', {}, network.name), ' ', el('span', { class: 'muted' }, `${online + 1}/${network.members.length + 1}`),
      el('div', { class: 'tags' }, tags)),
    manager && network.requests > 0 && el('button', {
      class: 'small', title: t('gui-requests'), onclick: stop(() => devicesDialog(network, 'requests')),
    }, icon('users'), el('span', { class: 'badge' }, network.requests)),
    manager && el('button', { class: 'icon', title: t('gui-invites'), onclick: stop(() => invitesDialog(network)) }, icon('link')),
    el('button', { class: 'icon', title: t('gui-network-settings'), onclick: stop(() => networkMenu(network)) }, icon('more')));
  const server = status.servers.find((candidate) => candidate.server === network.server);
  const toServer = serverLatency(server);
  const ping = (member) => {
    if (member.link === 'direct') return signal(member.latency_ms ?? null);
    if (member.link === 'relay') return signal(toServer == null ? null : toServer * 2, true);
    return signal(null);
  };
  const address = (value) => el('span', { class: 'mono address', title: t('gui-copy'), onclick: () => copy(value) }, value);
  const me = { nickname: status.nickname, link: server?.connection === 'connected' ? 'direct' : 'offline' };
  const self = el('div', { class: 'row self' },
    avatar(me),
    el('div', { class: 'grow' },
      el('div', {}, status.nickname, ' ', el('span', { class: 'tag' }, t('gui-you'))),
      el('div', { class: 'muted' }, several() ? serverName(server ?? {}) : t(`state-${server?.connection ?? 'disconnected'}`))),
    server?.address && address(server.address),
    signal(toServer),
    manager && el('span', { class: 'icon-gap' }));
  const rows = [self, network.members.map((member) => entering(el('div', { class: 'row' },
    avatar(member),
    el('div', { class: 'grow' },
      el('div', member.dns ? { class: 'dns-name', title: member.dns, onclick: () => copy(member.dns) } : {}, member.nickname),
      el('div', { class: 'muted' }, linkText(member))),
    address(member.address),
    ping(member),
    manager && el('button', { class: 'icon', onclick: () => memberDialog(network, member) }, icon('more'))),
  `member:${id}:${member.address}`))];
  card.append(head, el('div', { class: 'members' }, el('div', {}, rows)));
  return entering(card, `network:${id}`);
}

function notice(titleId, text, action = null, spinner = false) {
  return el('div', { class: 'card pad notice', 'data-key': `notice:${titleId}` },
    spinner && el('div', { class: 'spinner' }),
    el('h2', {}, t(titleId)), text && el('p', {}, text), action);
}

function problemCard() {
  if (problem.state === 'missing' || problem.state === 'denied') {
    const fix = el('button', {
      class: 'primary',
      onclick: async (event) => {
        const button = event.currentTarget;
        button.disabled = true;
        button.textContent = t('gui-repairing');
        try {
          await invoke('repair');
          toast(t('gui-repaired'));
        } catch (error) {
          toast(t('gui-repair-failed', { reason: String(error) }), true);
        }
        await poll();
      },
    }, t('gui-repair'));
    return notice(problem.state === 'denied' ? 'gui-repair-denied' : 'gui-repair-missing', t('gui-repair-hint'), fix);
  }
  return notice('gui-daemon-problem', problem.message, el('button', { onclick: () => poll() }, t('gui-retry')));
}

function content() {
  if (problem && Date.now() - startedAt > STARTUP_GRACE_MS) return [problemCard()];
  if (!status) return [notice('gui-starting', null, null, true)];
  if (!serverList(status).length) return [welcome()];
  const banner = update && el('div', { class: 'card update', 'data-key': 'update' },
    el('span', { class: 'grow' }, t('gui-update', { version: update.tag.replace(/^v/, '') })),
    el('button', { class: 'small primary', onclick: () => invoke('open_release', { url: update.url }).catch((e) => toast(String(e), true)) },
      t('gui-download')));
  const toolbar = el('div', { class: 'toolbar', 'data-key': 'toolbar' },
    el('button', { onclick: createDialog }, icon('plus'), t('gui-create-network')),
    el('button', { class: 'primary', onclick: joinDialog }, icon('join'), t('gui-join-network')));
  const networks = status.servers.map((server) => server.networks.map((network) => networkCard({ ...network, server: server.server })));
  const empty = status.servers.every((server) => !server.networks.length)
    && el('div', { class: 'card pad empty-state', 'data-key': 'empty' }, el('p', {}, t(overall(status) === 'connected' ? 'gui-no-networks' : 'gui-waiting-server')));
  return [banner, toolbar, networks, empty];
}

function render() {
  fresh = new Set();
  const headerNode = document.getElementById('header');
  morph(headerNode, Object.assign(header(), { id: 'header' }));
  const main = document.getElementById('main');
  morphChildren(main, el('main', {}, content()));
  seen = new Set([...seen, ...fresh]);
}

/* Polling */

async function poll() {
  clearTimeout(pollTimer);
  try {
    const response = await request('status');
    status = response.data;
    problem = null;
  } catch (error) {
    let state = null;
    try { state = await invoke('daemon_state'); } catch { state = null; }
    problem = { state, message: String(error) };
  }
  const editing = document.activeElement?.tagName === 'INPUT' && document.getElementById('main').contains(document.activeElement);
  if (!editing) render();
  pollTimer = setTimeout(poll, POLL_MS);
}

async function loadCatalog() {
  const catalog = await invoke('catalog');
  messages = catalog.messages;
  languages = catalog.languages;
  appVersion = catalog.version;
  document.documentElement.lang = catalog.language;
  document.documentElement.dir = catalog.rtl ? 'rtl' : 'ltr';
}

async function start() {
  await loadCatalog();
  try { settings = await invoke('settings'); } catch { /* defaults */ }
  startedAt = Date.now();
  render();
  await poll();
  checkUpdates();
  setInterval(checkUpdates, UPDATE_EVERY_MS);
}

start();
