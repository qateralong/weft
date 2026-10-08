'use strict';

const invoke = window.__TAURI__.core.invoke;
const POLL_MS = 1500;
const EXPIRY = [['gui-expiry-never', null], ['gui-expiry-hour', 3600], ['gui-expiry-day', 86400],
  ['gui-expiry-week', 7 * 86400], ['gui-expiry-month', 30 * 86400]];

let messages = {};
let status = null;
let daemonError = null;
let pollTimer = null;

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
  node.className = error ? 'error' : '';
  node.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { node.hidden = true; }, error ? 5000 : 2500);
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
  const close = () => dialog.close();
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

function renderHeader() {
  const header = document.getElementById('header');
  if (!status || !status.server) return header.replaceChildren();
  const connected = status.connection !== 'disconnected';
  header.replaceChildren(
    el('div', { class: 'me' },
      el('div', { class: 'name' }, status.nickname),
      el('div', { class: 'sub' },
        el('span', { class: `dot ${status.connection}` }),
        t(`state-${status.connection}`),
        status.address && el('span', { class: 'mono address', title: t('gui-copy'), onclick: () => copy(status.address) }, `· ${status.address}`))),
    el('button', { class: 'icon', title: t('gui-settings'), onclick: serverDialog }, '⚙'),
    el('button', {
      class: connected ? '' : 'primary',
      onclick: () => act(connected ? 'down' : 'up', connected ? {} : { link: null, nickname: null }),
    }, t(connected ? 'gui-disconnect' : 'gui-connect')));
}

function renderSetup() {
  const [linkLabel, link] = field('gui-server-link', { placeholder: 'weft://…' });
  const [nickLabel, nickname] = field('gui-nickname', { value: status?.nickname ?? '' });
  const connect = el('button', {
    class: 'primary',
    onclick: () => act('up', { link: link.value.trim(), nickname: nickname.value.trim() || null }),
  }, t('gui-connect'));
  return el('div', { class: 'card pad' },
    el('h2', {}, t('gui-setup-title')), el('p', {}, t('gui-setup-hint')), linkLabel, nickLabel, connect);
}

function renderNetwork(network) {
  const manager = network.role !== 'member';
  const tags = [t(`role-${network.role}`)];
  if (network.locked) tags.push(t('network-locked'));
  if (network.approval) tags.push(t('network-approval'));
  const head = el('div', { class: 'network-head' },
    el('div', { class: 'title' }, el('b', {}, network.name), el('div', { class: 'tags' }, tags.join(' · '))),
    manager && el('button', { class: 'small', onclick: () => invitesDialog(network.name) }, t('gui-invites')),
    manager && el('button', { class: 'small', onclick: () => devicesDialog(network.name, 'requests') },
      t('gui-requests'), network.requests > 0 && ' ', network.requests > 0 && el('span', { class: 'badge' }, network.requests)),
    el('button', { class: 'icon', title: t('gui-network-settings'), onclick: () => networkMenu(network) }, '⋯'));
  const rows = network.members.length
    ? network.members.map((member) => el('div', { class: 'row' },
      el('span', { class: `dot ${member.link}` }),
      el('span', { class: 'grow' }, member.nickname),
      el('span', { class: 'mono address', title: t('gui-copy'), onclick: () => copy(member.address) }, member.address),
      el('span', { class: 'muted link' }, linkText(member)),
      manager && el('button', { class: 'icon', onclick: () => memberDialog(network, member) }, '⋯')))
    : el('div', { class: 'row empty' }, t('network-empty'));
  return el('div', { class: 'card' }, head, rows);
}

function networkMenu(network) {
  const manager = network.role !== 'member';
  openDialog(network.name, ({ close }) => {
    const item = (label, run, style = '') => el('button', {
      class: style, style: 'width:100%;margin-bottom:8px;text-align:left',
      onclick: () => { close(); run(); },
    }, label);
    return [
      manager && item(t('gui-invites'), () => invitesDialog(network.name)),
      manager && item(t('gui-requests'), () => devicesDialog(network.name, 'requests')),
      manager && item(t('gui-bans'), () => devicesDialog(network.name, 'bans')),
      manager && item(t('gui-network-settings'), () => settingsDialog(network)),
      item(t('gui-leave'), () => act('leave', { name: network.name }, t('done-leave', { name: network.name })), 'danger'),
    ];
  });
}

function render() {
  renderHeader();
  const main = document.getElementById('main');
  if (daemonError) {
    main.replaceChildren(el('div', { class: 'card pad' }, el('h2', {}, 'weftd'), el('p', {}, daemonError)));
    return;
  }
  if (!status) return;
  if (!status.server) {
    main.replaceChildren(renderSetup());
    return;
  }
  const toolbar = el('div', { class: 'toolbar' },
    el('button', { onclick: createDialog }, t('gui-create-network')),
    el('button', { class: 'primary', onclick: joinDialog }, t('gui-join-network')));
  const networks = status.networks.length
    ? status.networks.map(renderNetwork)
    : status.connection === 'connected' && el('div', { class: 'card pad' }, el('p', {}, t('gui-no-networks')));
  main.replaceChildren(toolbar, ...nodes(networks));
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
