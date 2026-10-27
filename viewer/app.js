'use strict';

const TOKEN_KEY = 'marbles.view.token';
const SECTIONS = ['dashboard', 'queue', 'scrum', 'gantt'];

const state = {
  token: sessionStorage.getItem(TOKEN_KEY) || '',
  demo: false,
  filters: { project: '', folder: '', plan: '' },
  loading: false,
};

const $ = (id) => document.getElementById(id);

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = String(text);
  return node;
}

function svgElement(tag, attrs = {}) {
  const node = document.createElementNS('http://www.w3.org/2000/svg', tag);
  for (const [key, value] of Object.entries(attrs)) node.setAttribute(key, String(value));
  return node;
}

function number(value) {
  return Number.isFinite(Number(value)) ? Number(value).toLocaleString() : '—';
}

function generated(seconds) {
  if (!Number.isFinite(Number(seconds)) || Number(seconds) <= 0) return '';
  return `generated ${new Date(Number(seconds) * 1000).toLocaleString()}`;
}

function scopeText(scope) {
  if (!scope) return 'all work';
  const parts = [];
  if (scope.project) parts.push(`project ${scope.project}`);
  if (scope.folder) parts.push(`folder ${scope.folder}`);
  if (scope.plan) parts.push(`plan ${scope.plan}`);
  return parts.length ? parts.join(' · ') : 'all work';
}

function scopeBody(extra = {}) {
  const body = { ...extra };
  for (const [key, value] of Object.entries(state.filters)) {
    if (value.trim()) body[key] = value.trim();
  }
  return body;
}

function setSectionState(section, status, label, generatedAt) {
  const badge = document.querySelector(`[data-status="${section}"]`);
  if (badge) {
    badge.textContent = label;
    badge.className = `badge ${status === 'live' ? 'live' : status}`;
  }
  const stamp = document.querySelector(`[data-generated="${section}"]`);
  if (stamp) stamp.textContent = generated(generatedAt);
}

function setScope(section, text) {
  const node = document.querySelector(`[data-scope="${section}"]`);
  if (node) node.textContent = text;
}

function clearSection(section, message, kind = 'loading') {
  const body = $(`${section}-body`);
  if (!body) return;
  body.replaceChildren();
  if (!message) return;
  const stateNode = element('div', `state ${kind}`);
  stateNode.append(element('strong', null, kind === 'error' ? 'View unavailable' : 'Loading'));
  const detail = element('p', null, message);
  stateNode.append(detail);
  body.append(stateNode);
}

function showMessage(text, kind = 'info') {
  const banner = $('global-message');
  banner.textContent = text;
  banner.className = `banner ${kind}`;
  banner.hidden = !text;
}

function showTokenError(text) {
  const node = $('token-error');
  node.textContent = text || '';
  node.hidden = !text;
}

function updateAuthVisibility() {
  const auth = $('auth-panel');
  const filters = $('filter-panel');
  const hasToken = Boolean(state.token);
  auth.hidden = hasToken || state.demo;
  filters.hidden = !hasToken && !state.demo;
  $('session-chip').hidden = !hasToken;
  $('forget-token').hidden = !hasToken;
  $('demo-banner').hidden = !state.demo;
}

function rememberToken(token) {
  state.token = token.trim();
  if (state.token) sessionStorage.setItem(TOKEN_KEY, state.token);
  else sessionStorage.removeItem(TOKEN_KEY);
  $('token-input').value = '';
  updateAuthVisibility();
}

function forgetToken() {
  rememberToken('');
  showTokenError('Token cleared. Paste a bearer token to reconnect.');
}

async function fetchView(op) {
  const response = await fetch(`/v1/${op}`, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      authorization: `Bearer ${state.token}`,
    },
    body: JSON.stringify(scopeBody()),
    cache: 'no-store',
  });
  const text = await response.text();
  if (response.status === 401) {
    forgetToken();
    throw new Error('The server rejected this token. It may be expired or scoped to another company.');
  }
  if (!response.ok) {
    throw new Error(`HTTP ${response.status}: ${text.slice(0, 280).trim() || 'request failed'}`);
  }
  try {
    return JSON.parse(text);
  } catch {
    throw new Error('The server returned invalid JSON for this view.');
  }
}

function metricCard(label, value, note, kind) {
  const card = element('div', `metric ${kind || ''}`.trim());
  card.append(element('div', 'metric-label', label));
  card.append(element('div', 'metric-value', value));
  if (note) card.append(element('div', 'metric-note', note));
  return card;
}

function progressCard(done, total, percent) {
  const card = element('div', 'metric progress');
  card.append(element('div', 'metric-label', 'Progress'));
  card.append(element('div', 'metric-value', `${number(percent)}%`));
  card.append(element('div', 'metric-note', `${number(done)} of ${number(total)} closed`));
  const track = element('div', 'progress-track');
  const fill = element('span', 'progress-fill');
  fill.dataset.percent = String(Math.max(0, Math.min(100, Number(percent) || 0)));
  fill.style.width = `${Math.max(0, Math.min(100, Number(percent) || 0))}%`;
  track.append(fill);
  card.append(track);
  return card;
}

function tag(text, kind) {
  return element('span', `tag ${kind || ''}`.trim(), text);
}

function issueCard(card, variant) {
  const node = element('article', `issue-card ${variant || ''}`.trim());
  const titleRow = element('div', 'issue-title-row');
  titleRow.append(element('div', 'issue-title', card.title || card.id));
  titleRow.append(element('div', 'issue-id', card.id));
  node.append(titleRow);
  const meta = element('div', 'issue-meta');
  meta.append(tag(card.project || 'unknown project'));
  meta.append(tag(card.issue_type || 'task'));
  if (card.assignee) meta.append(tag(`claimed by ${card.assignee}`, 'agent'));
  if (card.blocked_by > 0) {
    meta.append(tag(`${number(card.blocked_by)} blocker${card.blocked_by === 1 ? '' : 's'}`, 'blocked-tag'));
  }
  if (card.estimate) meta.append(tag(`${number(card.estimate)}m estimate`));
  if (card.plan) meta.append(tag(`plan ${card.plan}`, 'plan'));
  if (card.age_seconds) meta.append(tag(`${Math.floor(card.age_seconds / 3600)}h old`));
  node.append(meta);
  return node;
}

function renderQueue(scrum) {
  const body = $('queue-body');
  body.replaceChildren();
  if (!scrum || !Array.isArray(scrum.columns)) {
    clearSection('queue', 'The dashboard did not include a scrum projection.', 'error');
    setSectionState('queue', 'error', 'unavailable', null);
    return;
  }
  const grid = element('div', 'queue-grid');
  const definitions = [
    ['open', 'Ready', 'ready'],
    ['blocked', 'Blocked', 'blocked'],
  ];
  for (const [status, title, kind] of definitions) {
    const column = scrum.columns.find((item) => item.status === status) || { cards: [] };
    const cards = Array.isArray(column.cards) ? column.cards : [];
    const queue = element('section', 'queue');
    const head = element('div', 'queue-head');
    const queueTitle = element('div', 'queue-title');
    queueTitle.append(element('strong', null, title));
    queueTitle.append(element('span', 'count-pill', number(cards.length)));
    head.append(queueTitle);
    queue.append(head);
    if (!cards.length) {
      queue.append(element('div', 'state empty', 'No work in this column.'));
    } else {
      const list = element('div', 'card-list');
      for (const card of cards) list.append(issueCard(card, status === 'open' ? kind : kind));
      queue.append(list);
    }
    grid.append(queue);
  }
  body.append(grid);
  setSectionState('queue', 'live', 'live', scrum.generated_at);
}

function renderDashboard(data) {
  const body = $('dashboard-body');
  body.replaceChildren();
  const totals = data.totals || {};
  const grid = element('div', 'metric-grid');
  grid.append(metricCard('Ready', number(totals.ready), 'open, unblocked, unclaimed', 'ready'));
  grid.append(metricCard('In progress', number(totals.in_progress), 'live claims'));
  grid.append(metricCard('Review', number(totals.review), 'waiting on a human', 'review'));
  grid.append(metricCard('Blocked', number(totals.blocked), 'dependency or hold', 'blocked'));
  grid.append(metricCard('Stale', number(totals.stale), `older than ${state.filters ? 'the stale window' : 'the stale window'}`, 'stale'));
  grid.append(progressCard(totals.closed, totals.total, totals.percent_done));
  body.append(grid);
  const note = element('p', 'hint');
  note.textContent = `${number(totals.total)} work item${Number(totals.total) === 1 ? '' : 's'} in scope. Goals are containers and are excluded.`;
  body.append(note);
  setSectionState('dashboard', 'live', 'live', data.generated_at);
  setScope('dashboard', scopeText(data.scope));
  renderQueue(data.scrum ? { ...data.scrum, generated_at: data.generated_at } : null);
}

function renderScrum(data) {
  const body = $('scrum-body');
  body.replaceChildren();
  const columns = Array.isArray(data.columns) ? data.columns : [];
  const swimlanes = Array.isArray(data.swimlanes) ? data.swimlanes : [];
  if (!columns.length && !swimlanes.length) {
    clearSection('scrum', 'No scrum columns were returned for this scope.', 'empty');
    setSectionState('scrum', 'live', 'live', data.generated_at);
    setScope('scrum', scopeText(data.scope));
    return;
  }
  const board = element('div', 'board');
  const lanes = swimlanes.length
    ? swimlanes
    : [{ key: 'all', title: 'All work', columns }];
  for (const lane of lanes) {
    const laneNode = element('section', 'lane');
    laneNode.append(element('div', 'lane-title', lane.title || lane.key || 'Work'));
    const columnsNode = element('div', 'columns');
    for (const column of lane.columns || []) {
      const columnNode = element('section', 'column');
      const head = element('div', 'column-head');
      const title = element('div', 'column-title');
      title.append(element('span', `status-dot ${column.status || ''}`.trim()));
      title.append(element('strong', null, column.title || column.status || 'Unknown'));
      head.append(title);
      const count = element('span', 'wip', number(column.count || 0));
      if (column.wip_limit && Number(column.count) > Number(column.wip_limit)) count.classList.add('over');
      head.append(count);
      columnNode.append(head);
      const cards = Array.isArray(column.cards) ? column.cards : [];
      if (!cards.length) {
        columnNode.append(element('div', 'column-empty', 'Empty'));
      } else {
        const list = element('div', 'card-list');
        for (const card of cards) list.append(issueCard(card, card.status === 'in_progress' ? 'in-progress' : card.status));
        columnNode.append(list);
      }
      columnsNode.append(columnNode);
    }
    laneNode.append(columnsNode);
    board.append(laneNode);
  }
  body.append(board);
  setSectionState('scrum', 'live', 'live', data.generated_at);
  setScope('scrum', `${scopeText(data.scope)} · ${number(columns.reduce((sum, column) => sum + Number(column.count || 0), 0))} cards`);
}

function ganttSvg(gantt) {
  const nodes = Array.isArray(gantt.nodes) ? gantt.nodes : [];
  const labelWidth = 220;
  const chartWidth = 760;
  const top = 46;
  const rowHeight = 30;
  const width = labelWidth + chartWidth + 34;
  const height = top + Math.max(1, nodes.length) * rowHeight + 32;
  const total = Math.max(1, ...nodes.map((node) => Number(node.finish_min) || 0));
  const x = (minute) => labelWidth + (Math.max(0, Number(minute) || 0) / total) * chartWidth;
  const svg = svgElement('svg', {
    class: 'gantt-svg',
    viewBox: `0 0 ${width} ${height}`,
    role: 'img',
    'aria-label': 'Gantt schedule with critical path',
  });

  const axis = svgElement('line', {
    x1: labelWidth,
    y1: top - 10,
    x2: labelWidth + chartWidth,
    y2: top - 10,
    class: 'gantt-axis-line',
  });
  svg.append(axis);

  const ticks = 6;
  for (let i = 0; i <= ticks; i += 1) {
    const minute = Math.round((total * i) / ticks);
    const tickX = x(minute);
    svg.append(
      svgElement('line', {
        x1: tickX,
        y1: top - 10,
        x2: tickX,
        y2: top + Math.max(1, nodes.length) * rowHeight,
        class: 'gantt-grid-line',
      }),
    );
    const label = svgElement('text', { x: tickX, y: top - 18, class: 'gantt-tick', 'text-anchor': 'middle' });
    label.textContent = `${minute}m`;
    svg.append(label);
  }

  const positions = new Map();
  nodes.forEach((node, index) => {
    const y = top + index * rowHeight;
    if (index % 2 === 1) {
      svg.append(svgElement('rect', { x: 0, y, width, height: rowHeight, class: 'gantt-row-even' }));
    }
    svg.append(svgElement('line', { x1: 0, y1: y + rowHeight, x2: width, y2: y + rowHeight, class: 'gantt-row-line' }));
    const key = svgElement('text', { x: 12, y: y + 13, class: 'gantt-label-key' });
    key.textContent = node.key || node.id;
    svg.append(key);
    const title = svgElement('text', { x: 12, y: y + 25, class: 'gantt-label-title' });
    title.textContent = String(node.title || '').slice(0, 38);
    svg.append(title);

    const barX = x(node.start_min);
    const barWidth = Math.max(2, x(node.finish_min) - barX);
    const critical = node.critical ? ' critical' : '';
    const bar = svgElement('rect', {
      x: barX,
      y: y + 8,
      width: barWidth,
      height: 14,
      rx: 3,
      class: `gantt-bar status-${node.status || 'open'}${critical}`,
    });
    const tooltip = `${node.title || node.id} · ${node.status || 'unknown'} · ${number(node.estimate)}m${node.critical ? ' · critical' : ''}`;
    bar.append(svgElement('title'));
    bar.firstChild.textContent = tooltip;
    svg.append(bar);
    positions.set(node.id, { x: barX, y: y + 15, endX: barX + barWidth });
  });

  const pathIds = Array.isArray(gantt.critical_path) ? gantt.critical_path : [];
  for (let i = 0; i < pathIds.length - 1; i += 1) {
    const from = positions.get(pathIds[i]);
    const to = positions.get(pathIds[i + 1]);
    if (!from || !to) continue;
    const mid = (from.endX + to.x) / 2;
    svg.append(
      svgElement('path', {
        d: `M ${from.endX} ${from.y} L ${mid} ${from.y} L ${mid} ${to.y} L ${to.x} ${to.y}`,
        class: 'gantt-critical-link',
      }),
    );
  }
  return svg;
}

function renderGantt(data) {
  const body = $('gantt-body');
  body.replaceChildren();
  const nodes = Array.isArray(data.nodes) ? data.nodes : [];
  const summary = element('div', 'gantt-summary');
  const stat = (label, value) => {
    const node = element('div', 'stat');
    node.append(element('div', 'stat-label', label));
    node.append(element('div', 'stat-value', value));
    return node;
  };
  summary.append(stat('Makespan', `${number(data.total_minutes)} min`));
  summary.append(stat('Critical path', `${number(data.critical_path_minutes)} min`));
  summary.append(stat('Running now', number(data.current_concurrency)));
  summary.append(stat('Peak concurrency', number(data.peak_concurrency)));
  body.append(summary);

  if (!nodes.length) {
    body.append(element('div', 'state empty', 'No schedulable work in this scope. Closed work is excluded by default.'));
  } else {
    const scroll = element('div', 'gantt-scroll');
    scroll.append(ganttSvg(data));
    body.append(scroll);
    const legend = element('div', 'gantt-legend');
    for (const [status, label] of [['open', 'Open'], ['in_progress', 'In progress'], ['review', 'Review'], ['blocked', 'Blocked']]) {
      const item = element('span', 'legend-item');
      item.append(element('span', `legend-swatch ${status}`));
      item.append(element('span', null, label));
      legend.append(item);
    }
    const critical = element('span', 'legend-item');
    critical.append(element('span', 'legend-swatch critical'));
    critical.append(element('span', null, 'Critical path'));
    legend.append(critical);
    body.append(legend);

    const chain = Array.isArray(data.critical_path) ? data.critical_path : [];
    if (chain.length) {
      const byId = new Map(nodes.map((node) => [node.id, node]));
      const criticalBox = element('div', 'critical-chain');
      criticalBox.append(element('h3', null, 'Critical chain'));
      const list = element('ol');
      for (const id of chain) {
        const node = byId.get(id);
        if (!node) continue;
        const item = element('li');
        item.append(element('span', null, node.key || node.id));
        item.append(element('span', 'chain-id', node.id));
        list.append(item);
      }
      criticalBox.append(list);
      body.append(criticalBox);
    }
  }
  setSectionState('gantt', 'live', 'live', data.generated_at);
  setScope('gantt', scopeText(data.scope));
}

function renderAll(payloads) {
  const { dashboard, scrum, gantt } = payloads;
  renderDashboard(dashboard);
  renderScrum(scrum);
  renderGantt(gantt);
}

function demoPayload() {
  const now = Math.floor(Date.now() / 1000);
  const cards = [
    { id: 'mb-201', title: 'View contract and read-only API', project: 'marbles', priority: 1, issue_type: 'task', assignee: null, labels: [], epic: null, blocked_by: 0, age_seconds: 7200, estimate: 45, plan: 'views-v1' },
    { id: 'mb-202', title: 'Scrum projection with WIP limits', project: 'marbles', priority: 1, issue_type: 'task', assignee: 'agent/view-worker', labels: ['viewer'], epic: null, blocked_by: 0, age_seconds: 5400, estimate: 30, plan: 'views-v1' },
    { id: 'mb-203', title: 'Gantt scheduling and critical path', project: 'marbles', priority: 2, issue_type: 'task', assignee: null, labels: ['viewer'], epic: null, blocked_by: 1, age_seconds: 3600, estimate: 60, plan: 'views-v1' },
    { id: 'mb-204', title: 'Canvas widget adapter', project: 'dock', priority: 2, issue_type: 'task', assignee: null, labels: ['canvas'], epic: null, blocked_by: 0, age_seconds: 1800, estimate: 90, plan: 'views-v1' },
    { id: 'mb-205', title: 'Ship review evidence', project: 'marbles', priority: 3, issue_type: 'task', assignee: 'human/avery', labels: [], epic: null, blocked_by: 0, age_seconds: 900, estimate: 20, plan: 'views-v1' },
  ];
  const columns = ['open', 'in_progress', 'review', 'blocked', 'deferred', 'closed'].map((status) => ({
    status,
    title: status === 'open' ? 'Ready' : status.replace('_', ' '),
    count: cards.filter((card) => status === 'open' && card.blocked_by === 0).length + (status === 'in_progress' ? 1 : status === 'review' ? 1 : status === 'blocked' ? 1 : 0),
    wip_limit: null,
    cards: cards.filter((card) => {
      if (status === 'open') return card.blocked_by === 0 && !card.assignee;
      if (status === 'in_progress') return card.assignee && card.id === 'mb-202';
      if (status === 'review') return card.id === 'mb-205';
      if (status === 'blocked') return card.blocked_by > 0;
      return false;
    }),
  }));
  const ganttNodes = [
    { id: 'mb-201', key: 'contract', title: 'View contract and read-only API', status: 'open', estimate: 45, start_min: 0, finish_min: 45, slack_min: 0, critical: true },
    { id: 'mb-202', key: 'scrum', title: 'Scrum projection with WIP limits', status: 'in_progress', estimate: 30, start_min: 0, finish_min: 30, slack_min: 0, critical: true },
    { id: 'mb-203', key: 'gantt', title: 'Gantt scheduling and critical path', status: 'blocked', estimate: 60, start_min: 45, finish_min: 105, slack_min: 0, critical: true },
    { id: 'mb-204', key: 'canvas', title: 'Canvas widget adapter', status: 'open', estimate: 90, start_min: 45, finish_min: 135, slack_min: 30, critical: false },
  ];
  const scrum = { schema: 'marbles.view.scrum.v1', generated_at: now, scope: {}, columns, swimlanes: [{ key: 'marbles', title: 'marbles', columns }] };
  const dashboard = {
    schema: 'marbles.view.dashboard.v1',
    generated_at: now,
    scope: {},
    totals: { total: 5, open: 3, in_progress: 1, review: 1, blocked: 1, closed: 0, ready: 2, stale: 0, percent_done: 0 },
    portfolio: { generated_at: now, days: 14, stale_days: 7, folders: [], unfiled: [], totals: { projects: 2, progress: {}, ready: 2, stale: 0 } },
    burndown: { days: 14, scope: 5, done: 0, remaining: 5, points: [] },
    scrum,
    gantt: { plan: 'views-v1', started_at: now, nodes: ganttNodes, critical_path: ['mb-201', 'mb-203'], critical_path_minutes: 105, total_minutes: 135, current_concurrency: 1, peak_concurrency: 2 },
  };
  const gantt = { scope: {}, default_estimate_min: 60, plan: 'views-v1', started_at: now, nodes: ganttNodes, critical_path: ['mb-201', 'mb-203'], critical_path_minutes: 105, total_minutes: 135, current_concurrency: 1, peak_concurrency: 2 };
  return { dashboard, scrum, gantt };
}

function renderDemo() {
  showMessage('', 'info');
  const payload = demoPayload();
  for (const section of SECTIONS) setSectionState(section, 'demo', 'demo', payload.dashboard.generated_at);
  renderAll(payload);
  for (const section of SECTIONS) setSectionState(section, 'demo', 'demo', payload.dashboard.generated_at);
  $('last-refresh').textContent = 'Demo data loaded. No request was made.';
}

async function refresh() {
  if (state.loading) return;
  if (state.demo) {
    renderDemo();
    return;
  }
  if (!state.token) {
    updateAuthVisibility();
    clearSection('dashboard', 'Paste a bearer token to load the tracker.', 'empty');
    return;
  }
  state.loading = true;
  showMessage('', 'info');
  $('refresh-button').disabled = true;
  for (const section of SECTIONS) {
    setSectionState(section, 'loading', 'loading', null);
    clearSection(section, 'Fetching the read-only view APIs…');
  }
  try {
    const results = await Promise.allSettled([fetchView('views.dashboard'), fetchView('views.scrum'), fetchView('views.gantt')]);
    const [dashboard, scrum, gantt] = results;
    let failures = 0;
    let unauthorized = false;
    const failure = (result) => {
      if (result.status === 'rejected') {
        failures += 1;
        if (String(result.reason && result.reason.message).includes('rejected this token')) unauthorized = true;
        return result.reason;
      }
      return null;
    };
    const dashboardError = failure(dashboard);
    const scrumError = failure(scrum);
    const ganttError = failure(gantt);
    if (dashboardError) {
      clearSection('dashboard', dashboardError.message, 'error');
      setSectionState('dashboard', 'error', 'unavailable', null);
    } else {
      renderDashboard(dashboard.value);
    }
    if (scrumError) {
      clearSection('scrum', scrumError.message, 'error');
      setSectionState('scrum', 'error', 'unavailable', null);
    } else {
      renderScrum(scrum.value);
    }
    if (ganttError) {
      clearSection('gantt', ganttError.message, 'error');
      setSectionState('gantt', 'error', 'unavailable', null);
    } else {
      renderGantt(gantt.value);
    }
    if (dashboardError) {
      clearSection('queue', 'The dashboard aggregate is unavailable, so ready and blocked work cannot be shown.', 'error');
      setSectionState('queue', 'error', 'unavailable', null);
    }
    if (unauthorized) {
      updateAuthVisibility();
      showTokenError('The server rejected this token. Paste a current token.');
    } else if (failures) {
      showMessage(`${failures} view${failures === 1 ? '' : 's'} failed. Cached or partial data is not shown as current.`, 'warn');
    } else {
      $('last-refresh').textContent = `Last successful refresh ${new Date().toLocaleTimeString()}.`;
    }
  } finally {
    state.loading = false;
    $('refresh-button').disabled = false;
  }
}

function readFilters(event) {
  if (event) event.preventDefault();
  state.filters = {
    project: $('filter-project').value,
    folder: $('filter-folder').value,
    plan: $('filter-plan').value,
  };
  refresh();
}

function init() {
  $('token-form').addEventListener('submit', (event) => {
    event.preventDefault();
    const value = $('token-input').value.trim();
    if (!value) {
      showTokenError('Enter a bearer token.');
      return;
    }
    showTokenError('');
    rememberToken(value);
    refresh();
  });
  $('forget-token').addEventListener('click', () => {
    forgetToken();
    refresh();
  });
  $('demo-toggle').addEventListener('change', (event) => {
    state.demo = event.target.checked;
    updateAuthVisibility();
    if (state.demo) renderDemo();
    else refresh();
  });
  $('filter-form').addEventListener('submit', readFilters);
  $('refresh-button').addEventListener('click', refresh);
  updateAuthVisibility();
  if (state.token) refresh();
  else clearSection('dashboard', 'Paste a bearer token, or turn on Demo data to review the shell.', 'empty');
}

document.addEventListener('DOMContentLoaded', init);
