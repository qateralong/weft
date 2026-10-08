'use strict';

const invoke = window.__TAURI__.core.invoke;
const POLL_MS = 1500;
const EXPIRY = [['gui-expiry-never', null], ['gui-expiry-hour', 3600], ['gui-expiry-day', 86400],
  ['gui-expiry-week', 7 * 86400], ['gui-expiry-month', 30 * 86400]];

let messages = {};
let status = null;
let daemonError = null;
let pollTimer = null;

const ICONS = {
  power: '<path d="M12 3v9"/><path d="M6.4 6.6a8 8 0 1 0 11.2 0"/>',
  sun: '<circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/>',
  moon: '<path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z"/>',
  gear: '<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  join: '<path d="M15 3h4a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2h-4"/><path d="M10 17l5-5-5-5"/><path d="M15 12H3"/>',
  chevron: '<path d="M6 9l6 6 6-6"/>',
  more: '<circle cx="5" cy="12" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/>',
  link: '<path d="M10 13a5 5 0 0 0 7.5.5l3-3a5 5 0 0 0-7-7l-1.7 1.7"/><path d="M14 11a5 5 0 0 0-7.5-.5l-3 3a5 5 0 0 0 7 7l1.7-1.7"/>',
  pulse: '<path d="M22 12h-4l-3 9L9 3l-3 9H2"/>',
  users: '<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M19 8v6M22 11h-6"/>',
};
const AVATAR_COLORS = ['#d9734e', '#c9a03a', '#5f9e57', '#3e9a91', '#3f7fb8', '#7867c4', '#b95f9d', '#8b7258'];

function icon(name) {
  const span = document.createElement('span');
  span.style.display = 'contents';
  span.innerHTML = `<svg viewBox="0 0 24 24">${ICONS[name]}</svg>`;
  return span;
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
  renderHeader();
}

applyTheme(stored('theme', matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light'));

const t = (id, args = {}) => (messages[id] ?? id).replace(/\{\$(\w+)\}/g, (_, name) => args[name] ?? '');

function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key === 'class') node.className = value;
    else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
    else if (key in node) node[key] = value;
    else node.setAttribute(key, value);
  }
  for (const child of nodes(children)) node.append(child instanceof Node ? child : String(child));
  return node;
}

const nodes = (value) => [value].flat(Infinity).filter((node) => node != null && node !== false);

const request = (command, fields = {}) => invoke('request', { request: { command, ...fields } });

let toastTimer = null;
function toast(text, error = false) {
  const node = document.getElementById('toast');
  node.textContent = text;
  node.className = error ? 'error show' : 'show';
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => node.classList.remove('show'), error ? 5000 : 2500);
}

async function act(command, fields, done) {
  try {
    const response = await request(command, fields);
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
    case 'direct': return member.latency_ms != null ? t('link-direct-latency', { ms: member.latency_ms }) : t('link-direct');
    case 'relay': return t('link-relay');
    case 'connecting': return t('link-connecting');
    default: return t('link-offline');
  }
}

/* Dialogs */

function openDialog(title, build, actions = []) {
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
  const buttons = actions.map(({ label, primary, run }) => el('button', {
    class: primary ? 'primary' : '',
    onclick: async (event) => {
      error.hidden = true;
      event.target.disabled = true;
      try {
        if (await run({ close, fail, body }) !== false) close();
      } catch (e) {
        fail(String(e));
      } finally {
        event.target.disabled = false;
      }
    },
  }, label));
  dialog.replaceChildren(el('h3', {}, title), body,
    el('div', { class: 'actions' }, el('button', { onclick: close }, t(actions.length ? 'gui-cancel' : 'gui-close')), buttons));
  dialog.oncancel = (event) => { event.preventDefault(); close(); };
  dialog.onkeydown = (event) => {
    if (event.key === 'Enter' && event.target.tagName === 'INPUT' && buttons.length) buttons[buttons.length - 1].click();
  };
  render().then(() => {
    dialog.showModal();
    body.querySelector('input')?.focus();
  });
}

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
  openDialog(t('gui-diagnostics'), async ({ fail, render }) => {
    try {
      text = await invoke('diagnostics');
    } catch (e) {
      fail(String(e));
    }
    return [
      el('pre', { class: 'mono report' }, text),
      el('div', { class: 'toolbar' },
        el('button', { onclick: () => render() }, t('gui-refresh')),
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

function serverDialog() {
  const [linkLabel, link] = field('gui-server-link', { value: status?.server ?? '', placeholder: 'weft://…' });
  const [nickLabel, nickname] = field('gui-nickname', { value: status?.nickname ?? '' });
  openDialog(t('gui-settings'), () => [linkLabel, nickLabel], [{
    label: t('gui-save'), primary: true,
    run: async () => {
      await request('up', { link: link.value.trim() || null, nickname: nickname.value.trim() || null });
      await poll();
    },
  }]);
}

function createDialog() {
  const [nameLabel, name] = field('gui-network-name', { maxLength: 64 });
  const [passLabel, password] = field('gui-password', { type: 'password' });
  const [repeatLabel, repeat] = field('gui-password-repeat', { type: 'password' });
  openDialog(t('gui-create-network'), () => [nameLabel, passLabel, repeatLabel], [{
    label: t('gui-create'), primary: true,
    run: async ({ fail }) => {
      if (password.value !== repeat.value) return fail(t('password-mismatch')) ?? false;
      await request('create', { name: name.value.trim(), password: password.value });
      toast(t('done-create', { name: name.value.trim() }));
      await poll();
    },
  }]);
}

function joinDialog() {
  const [targetLabel, target] = field('gui-name-or-link', { placeholder: 'weft://…' });
  const [passLabel, password] = field('gui-password', { type: 'password' });
  const update = () => { passLabel.hidden = target.value.trim().toLowerCase().startsWith('weft://'); };
  target.addEventListener('input', update);
  openDialog(t('gui-join-network'), () => [targetLabel, passLabel], [{
    label: t('gui-join'), primary: true,
    run: async () => {
      const value = target.value.trim();
      const response = passLabel.hidden
        ? await request('redeem', { link: value })
        : await request('join', { name: value, password: password.value });
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

function invitesDialog(network) {
  const [usesLabel, uses] = field('gui-invite-uses', { type: 'number', min: 1 });
  const expiry = el('select', {}, EXPIRY.map(([id, seconds], index) =>
    el('option', { value: seconds ?? '', selected: index === 3 }, t(id))));
  let created = null;
  openDialog(t('gui-invites'), async ({ fail, render }) => {
    const create = el('button', {
      class: 'primary',
      onclick: async () => {
        try {
          const response = await request('create_invite', {
            network, uses: uses.value ? Number(uses.value) : null,
            expires_in: expiry.value ? Number(expiry.value) : null,
          });
          created = response.data;
          await render();
        } catch (e) { fail(String(e)); }
      },
    }, t('gui-create'));
    const result = created && el('div', { class: 'list' }, el('div', { class: 'row' },
      el('span', { class: 'grow mono' }, created.link ?? created.code),
      el('button', { class: 'small', onclick: () => copy(created.link ?? created.code) }, t('gui-copy'))));
    let list;
    try {
      const invites = (await request('invites', { network })).data;
      list = invites.length
        ? invites.map((invite) => el('div', { class: 'row' },
          el('div', { class: 'grow wrap' }, el('div', { class: 'mono' }, invite.code), el('div', { class: 'muted' }, inviteDetails(invite))),
          el('button', { class: 'small', onclick: () => copy(invite.link ?? invite.code) }, t('gui-copy')),
          el('button', {
            class: 'small danger',
            onclick: async () => {
              try { await request('revoke_invite', { code: invite.code }); await render(); } catch (e) { fail(String(e)); }
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
      create, result && el('div', { style: 'margin-top:10px' }, result),
      el('div', { class: 'section' }, t('invites-title', { name: network })),
      el('div', { class: 'list' }, list),
    ];
  });
}

function devicesDialog(network, kind) {
  const actions = kind === 'requests'
    ? [['gui-approve', 'approve', 'primary'], ['gui-deny', 'deny', 'danger']]
    : [['gui-unban', 'unban', '']];
  openDialog(t(kind === 'requests' ? 'gui-requests' : 'gui-bans'), async ({ fail, render }) => {
    let devices = [];
    try {
      devices = (await request(kind, { network })).data;
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
            await request(command, { network, member: device.public_key });
            await poll();
            await render();
          } catch (e) { fail(String(e)); }
        },
      }, t(label))))));
  });
}

function settingsDialog(network) {
  openDialog(t('gui-network-settings'), ({ fail, close }) => {
    const current = status.networks.find((n) => n.name === network.name) ?? network;
    const owner = current.role === 'owner';
    const toggle = (labelId, key) => {
      const box = el('input', {
        type: 'checkbox', checked: current[key],
        onchange: async () => {
          try {
            await request('configure', { network: network.name, locked: null, approval: null, password: null, [key]: box.checked });
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
            await request('configure', { network: network.name, locked: null, approval: null, password: password.value });
            password.value = '';
            toast(t('done-password', { name: network.name }));
          } catch (e) { fail(String(e)); }
        },
      }, t('gui-change-password')));
      parts.push(el('div', { class: 'section' }, t('gui-delete-network')), dangerButton(t('gui-delete-network'), async () => {
        try {
          await request('delete', { network: network.name });
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
    el('div', { class: 'toolbar', style: 'flex-wrap:wrap' }, actions.map(([label, command, done, role]) => el('button', {
      class: command === 'set_role' ? '' : 'danger',
      onclick: async () => {
        try {
          const fields = { network: network.name, member: member.address };
          if (role) fields.role = role;
          await request(command, fields);
          toast(t(done, { name: network.name, member: member.nickname }));
          close();
          await poll();
        } catch (e) { fail(String(e)); }
      },
    }, t(label)))),
  ]);
}

/* Rendering */

let seen = new Set();
let fresh = new Set();
const collapsed = new Set(JSON.parse(stored('collapsed', '[]')));

function entering(node, key) {
  fresh.add(key);
  if (!seen.has(key)) {
    node.classList.add('enter');
    node.style.animationDelay = `${Math.min(fresh.size, 12) * 35}ms`;
  }
  return node;
}

function avatar(member) {
  let hash = 0;
  for (const char of member.nickname) hash = (hash * 31 + char.codePointAt(0)) >>> 0;
  const node = el('div', { class: 'avatar' }, [...member.nickname][0] ?? '?', el('span', { class: `dot ${member.link}` }));
  node.style.background = AVATAR_COLORS[hash % AVATAR_COLORS.length];
  return node;
}

function renderHeader() {
  const header = document.getElementById('header');
  if (!status || !status.server) return header.replaceChildren();
  const connected = status.connection !== 'disconnected';
  const dark = document.documentElement.dataset.theme === 'dark';
  header.replaceChildren(
    el('button', {
      class: `power ${status.connection}`,
      title: t(connected ? 'gui-disconnect' : 'gui-connect'),
      onclick: () => act(connected ? 'down' : 'up', connected ? {} : { link: null, nickname: null }),
    }, icon('power')),
    el('div', { class: 'me' },
      el('div', { class: 'name' }, status.nickname),
      el('div', { class: 'sub' },
        el('span', { class: `dot ${status.connection}` }),
        t(`state-${status.connection}`),
        status.address && el('span', { class: 'mono address', title: t('gui-copy'), onclick: () => copy(status.address) }, status.address))),
    el('button', { class: 'icon', title: t('gui-diagnostics'), onclick: diagnosticsDialog }, icon('pulse')),
    el('button', { class: 'icon', title: t(dark ? 'gui-theme-light' : 'gui-theme-dark'), onclick: toggleTheme }, icon(dark ? 'sun' : 'moon')),
    el('button', { class: 'icon', title: t('gui-settings'), onclick: serverDialog }, icon('gear')));
}

function renderSetup() {
  const [linkLabel, link] = field('gui-server-link', { placeholder: 'weft://…' });
  const [nickLabel, nickname] = field('gui-nickname', { value: status?.nickname ?? '' });
  const connect = el('button', {
    class: 'primary',
    onclick: () => act('up', { link: link.value.trim(), nickname: nickname.value.trim() || null }),
  }, icon('power'), t('gui-connect'));
  return el('div', { class: 'card hero enter' },
    el('img', { src: 'icon.png', alt: '' }),
    el('h2', {}, t('gui-setup-title')), el('p', {}, t('gui-setup-hint')), linkLabel, nickLabel, connect);
}

function toggleNetwork(card, name) {
  if (collapsed.has(name)) collapsed.delete(name); else collapsed.add(name);
  card.classList.toggle('collapsed', collapsed.has(name));
  store('collapsed', JSON.stringify([...collapsed]));
}

function renderNetwork(network) {
  const manager = network.role !== 'member';
  const tags = [el('span', { class: 'tag role' }, t(`role-${network.role}`))];
  if (network.locked) tags.push(el('span', { class: 'tag' }, t('network-locked')));
  if (network.approval) tags.push(el('span', { class: 'tag' }, t('network-approval')));
  const online = network.members.filter((member) => member.link !== 'offline').length;
  const stop = (run) => (event) => { event.stopPropagation(); run(); };
  const card = el('div', { class: `card${collapsed.has(network.name) ? ' collapsed' : ''}` });
  const head = el('div', { class: 'network-head', onclick: () => toggleNetwork(card, network.name) },
    el('span', { class: 'chevron' }, icon('chevron')),
    el('div', { class: 'title' },
      el('b', {}, network.name), ' ', el('span', { class: 'muted' }, `${online + 1}/${network.members.length + 1}`),
      el('div', { class: 'tags' }, tags)),
    manager && network.requests > 0 && el('button', {
      class: 'small', title: t('gui-requests'), onclick: stop(() => devicesDialog(network.name, 'requests')),
    }, icon('users'), el('span', { class: 'badge' }, network.requests)),
    manager && el('button', { class: 'icon', title: t('gui-invites'), onclick: stop(() => invitesDialog(network.name)) }, icon('link')),
    el('button', { class: 'icon', title: t('gui-network-settings'), onclick: stop(() => networkMenu(network)) }, icon('more')));
  const rows = network.members.length
    ? network.members.map((member) => entering(el('div', { class: 'row' },
      avatar(member),
      el('div', { class: 'grow' }, el('div', {}, member.nickname), el('div', { class: 'muted link-text' }, linkText(member))),
      el('span', { class: 'mono address', title: t('gui-copy'), onclick: () => copy(member.address) }, member.address),
      manager && el('button', { class: 'icon', onclick: () => memberDialog(network, member) }, icon('more'))),
    `member:${network.name}:${member.address}`))
    : el('div', { class: 'row empty' }, t('network-empty'));
  card.append(head, el('div', { class: 'members' }, el('div', {}, rows)));
  return entering(card, `network:${network.name}`);
}

function networkMenu(network) {
  const manager = network.role !== 'member';
  openDialog(network.name, ({ close }) => {
    const item = (label, run, style = '') => el('button', {
      class: style,
      onclick: () => { close(); setTimeout(run, 170); },
    }, label);
    return el('div', { class: 'menu' },
      manager && item(t('gui-invites'), () => invitesDialog(network.name)),
      manager && item(t('gui-requests'), () => devicesDialog(network.name, 'requests')),
      manager && item(t('gui-bans'), () => devicesDialog(network.name, 'bans')),
      manager && item(t('gui-network-settings'), () => settingsDialog(network)),
      item(t('gui-leave'), () => act('leave', { name: network.name }, t('done-leave', { name: network.name })), 'danger'));
  });
}

function render() {
  fresh = new Set();
  renderHeader();
  const main = document.getElementById('main');
  if (daemonError) {
    main.replaceChildren(el('div', { class: 'card pad enter' }, el('h2', {}, 'weftd'), el('p', {}, daemonError)));
    return;
  }
  if (!status) return;
  if (!status.server) {
    main.replaceChildren(renderSetup());
    return;
  }
  const toolbar = el('div', { class: 'toolbar' },
    el('button', { onclick: createDialog }, icon('plus'), t('gui-create-network')),
    el('button', { class: 'primary', onclick: joinDialog }, icon('join'), t('gui-join-network')));
  const networks = status.networks.length
    ? status.networks.map(renderNetwork)
    : status.connection === 'connected' && el('div', { class: 'card pad enter' }, el('p', {}, t('gui-no-networks')));
  main.replaceChildren(toolbar, ...nodes(networks));
  seen = fresh;
}

function sameStatus(a, b) {
  return JSON.stringify(a) === JSON.stringify(b);
}

async function poll() {
  clearTimeout(pollTimer);
  try {
    const response = await request('status');
    const changed = daemonError || !sameStatus(status, response.data);
    status = response.data;
    daemonError = null;
    const editing = !status.server && document.activeElement?.tagName === 'INPUT';
    if (changed && !editing) render();
  } catch (error) {
    if (daemonError !== String(error)) {
      daemonError = String(error);
      status = null;
      render();
    }
  }
  pollTimer = setTimeout(poll, POLL_MS);
}

async function start() {
  const catalog = await invoke('catalog');
  messages = catalog.messages;
  document.documentElement.lang = catalog.language;
  await poll();
}

start();
