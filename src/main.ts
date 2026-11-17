import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { snapshotEvent } from '../contracts';
import type { Commands, DeviceSnapshot, ModuleId, PairRequest } from '../contracts';
import './style.css';
import { t, html, locale, setLanguage } from './i18n';
import type { Settings, Appearance } from '../contracts';
type Page = 'overview' | 'modules' | 'devices' | 'activity' | 'settings';
const app = document.querySelector<HTMLDivElement>('#app')!;
const dialogRoot = document.querySelector<HTMLDivElement>('#dialog-root')!;
const notice = document.querySelector<HTMLDivElement>('#notice')!;
const pages: Record<Page, string> = { overview: '概览', modules: '能力管理', devices: '设备与授权', activity: '活动', settings: '设置' };
const icons = { overview: '◫', modules: '◇', devices: '⇄', activity: '≋', settings: '⚙' };
const moduleCopy: Record<ModuleId, {
    title: string;
    subtitle: string;
    icon: string;
    detail: string;
}> = {
    system: { title: '系统工具', subtitle: '文件 · 搜索 · 命令', icon: '⌘', detail: '开启后，已配对 Maxa 可以使用本机文件、搜索和命令能力。不设置目录白名单。' },
    windows: { title: 'Windows 控制', subtitle: '窗口 · 截图 · 输入', icon: '▦', detail: '操作当前用户的交互桌面。需要已登录并解锁；你随时可以接管或停止控制。' },
    mobile: { title: 'Android 控制', subtitle: '连接 · 观察 · 操作', icon: '▯', detail: '通过这台电脑连接 Android。开启模块不会自动授予手机调试或输入权限。' },
};
const labels: Record<string, string> = {
    ready: '就绪', installed: '已安装', starting: '启动中', disabled: '未开启', failed: '失败', stopped: '已停止',
    queued: '等待中', running: '运行中', succeeded: '已完成', timed_out: '已超时', cancel_requested: '取消待确认',
    cancelled: '已取消', interrupted: '已中断', unknown: '结果未知', offline: '离线', unauthorized: '未授权',
    online: '在线', 'observe-only': '仅可观察', 'control-ready': '可控制', busy: '使用中',
};
let snapshot: DeviceSnapshot | null = null;
let page: Page = 'overview';
let failure = '';
let settings: Settings | null = null;
const systemTheme = matchMedia('(prefers-color-scheme: dark)');
function applyPreferences() {
    const appearance = settings?.appearance;
    setLanguage(appearance?.language ?? 'system');
    document.documentElement.dataset.theme = appearance?.theme === 'system' || !appearance ? (systemTheme.matches ? 'dark' : 'light') : appearance.theme;
}
systemTheme.addEventListener('change', applyPreferences);
applyPreferences();
let booting = true;
let noticeTimer: ReturnType<typeof setTimeout> | undefined;
const busy = new Set<string>();
const escape = (value: unknown) => String(value ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]!);
const short = (value: string) => value.length > 20 ? `${value.slice(0, 9)}…${value.slice(-7)}` : value;
const badge = (state: string) => `<span class="badge ${['ready', 'online', 'control-ready', 'succeeded'].includes(state) ? 'positive' : ['failed', 'unknown', 'unauthorized'].includes(state) ? 'negative' : ''}"><i></i>${escape(labels[state] ? t(labels[state]) : state)}</span>`;
const empty = (title: string, text: string) => `<div class="empty"><span class="empty-symbol">◇</span><strong>${escape(title)}</strong><p>${escape(text)}</p></div>`;
const time = (value: number) => new Date(value).toLocaleString(locale(), { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' });
const disabled = (key: string) => busy.has(key) ? ' disabled aria-busy="true"' : '';
function announce(text: string) {
    notice.textContent = text;
    notice.classList.add('visible');
    clearTimeout(noticeTimer);
    noticeTimer = setTimeout(() => notice.classList.remove('visible'), 4200);
}
async function command<K extends keyof Commands>(name: K, args: Commands[K]['args']): Promise<Commands[K]['result']> {
    return invoke<Commands[K]['result']>(name, args);
}
async function operate<K extends keyof Commands>(key: string, name: K, args: Commands[K]['args']) {
    if (busy.has(key))
        throw new Error(t("该操作正在进行"));
    busy.add(key);
    failure = '';
    render();
    try {
        const result = await command(name, args);
        if (result && typeof result === 'object' && 'protocolVersion' in result)
            snapshot = result as DeviceSnapshot;
        return result;
    }
    catch (error) {
        failure = String(error);
        throw error;
    }
    finally {
        busy.delete(key);
        render();
    }
}
const handle = (work: Promise<unknown>) => { void work.catch(() => { }); };
function moduleCards() {
    return (['system', 'windows', 'mobile'] as ModuleId[]).map(id => {
        const data = snapshot?.modules.find(item => item.id === id);
        const copy = moduleCopy[id];
        const enabled = data?.enabled ?? false;
        return `<article class="module-card"><div class="card-top"><span class="module-icon">${copy.icon}</span>${badge(data?.phase ?? 'disabled')}</div>
      <h3>${t(copy.title)}</h3><div class="eyebrow">${t(copy.subtitle)}</div><p>${t(copy.detail)}</p>
      ${data?.error ? `<div class="inline-error">${escape(data.error)}</div>` : ''}${enabled && data?.phase === 'failed' ? html `<button class="button subtle" data-action="module-retry" data-id="${id}"${disabled(`module:${id}`)}>重试启动</button>` : ''}
      <div class="card-bottom"><span class="version">${data?.version ? escape(data.version) : t("启用时检查依赖")}</span><button class="switch" role="switch" aria-label="${t(copy.title)}" aria-checked="${Boolean(enabled)}" data-action="module" data-id="${id}"${disabled(`module:${id}`)}${snapshot ? '' : ' disabled'}><span></span></button></div></article>`;
    }).join('');
}
function overview() {
    const clients = snapshot?.clients.filter(client => !client.revoked).length ?? 0;
    const running = snapshot?.jobs.filter(job => ['queued', 'running', 'cancel_requested'].includes(job.state)).length ?? 0;
    return html `<section class="hero"><div><div class="eyebrow">你的电脑，由你授权</div><h1>连接 Agent，<br>保留控制权。</h1><p>开启所需能力，再把这台设备交给可信的 Maxa 客户端。</p><button class="button primary" data-page="devices">管理连接 <span>↗</span></button></div><div class="host-art" aria-hidden="true"><div class="host-screen"><span>M</span><div class="host-status"><i></i> MAXA DEVICE</div></div><div class="host-foot"></div></div></section>
    <section class="stats"><div><strong>${clients}</strong><span>已授权客户端</span></div><div><strong>${(snapshot?.phones.filter(phone => !phone.removed).length ?? 0)}</strong><span>已添加 Android</span></div><div><strong>${running}</strong><span>进行中的任务</span></div></section>
    <div class="section-heading"><h2>按需开启能力</h2><span>已配对客户端可使用开启的模块</span></div><section class="module-grid">${moduleCards()}</section>
    <div class="info-line"><span>◈</span><p>配对代表信任，不是目录沙箱。关闭模块或解除配对可持续禁止访问；停止按钮只取消当前操作，后续新请求可继续。</p></div>`;
}
function modules() {
    return html `<div class="page-heading"><div class="eyebrow">CAPABILITIES</div><h1>只开启需要的能力</h1><p>模块独立启停。依赖保存在 App 私有目录，不替换你的全局开发环境。</p></div><section class="module-grid">${moduleCards()}</section>
    <section class="panel"><div class="panel-title"><h2>运行边界</h2><span class="muted">你开启多少，就能使用多少</span></div><div class="boundary-list"><div><b>系统工具</b><p>文件与命令使用本机用户权限，不设目录授权。远程文件使用绝对路径；关闭模块会拒绝新调用并取消其受管工作。</p></div><div><b>Windows 控制</b><p>只作用于当前活动用户桌面，不绕过锁屏或 UAC。没有控制租期；停止当前操作后，新请求仍可继续。</p></div><div><b>Android 控制</b><p>添加后，新请求自动尝试连接并准备控制，无需手动恢复。关闭模块或移除设备可持续禁用；手机系统权限仍有效。</p></div></div></section>`;
}
function devices() {
    const clients = snapshot?.clients.filter(client => !client.revoked) ?? [];
    const requests = snapshot?.pairing ?? [];
    const phones = snapshot?.phones.filter(phone => !phone.removed) ?? [];
    const removedPhones = snapshot?.phones.filter(phone => phone.removed) ?? [];
    return html `<div class="page-heading"><div class="eyebrow">CONNECTIONS</div><h1>认识每一个连接</h1><p>从 Maxa 发起申请，在这里批准。配对长期有效，你随时可以撤销。</p></div>
    <section class="panel"><div class="panel-title"><h2>连接这台设备</h2><span class="tag">由 Maxa 主动申请</span></div><p>在 MaxaConfig 的设备页选择局域网设备，或输入这台电脑的可达 IP:Port。收到申请后，核对双方显示的核对码并批准。</p><p class="hint">服务监听：<code>${escape(snapshot?.service.address || t("尚未就绪"))}</code>。监听地址不一定能从其他机器访问；跨网段需要可达地址，不提供自动穿透。</p><div class="info-line"><span>◈</span><p>批准的是可信客户端，不是单个目录。它可使用当前及以后开启的模块；关闭模块、移除手机或解除配对可持续禁止访问。停止只取消当前操作。</p></div></section>
    ${(requests.length ? html `<section class="panel requests"><div class="panel-title"><h2>等待你确认</h2><span class="count">${requests.length}</span></div>${requests.map(request => html `<div class="list-row"><div><strong>${escape(request.clientName)}</strong><p class="muted">申请于 ${time(request.createdAt)}</p><code>${escape(short(request.clientIdentity))}</code></div><button class="button primary" data-action="review" data-id="${escape(request.id)}">核对并批准</button></div>`).join('')}</section>` : '')}
    <section class="panel"><div class="panel-title"><h2>已授权客户端</h2><span class="muted">${clients.length} 个长期配对</span></div>${(clients.length ? clients.map(client => html `<div class="list-row"><div><strong>${escape(client.name)}</strong><p class="muted">可使用 App 已开启的模块 · 无控制租期</p><code title="${escape(client.id)}">${escape(short(client.id))}</code></div><button class="button subtle danger-text" data-action="revoke" data-id="${escape(client.id)}"${disabled(`revoke:${client.id}`)}>解除配对</button></div>`).join('') : empty(t("还没有已授权客户端"), t("请从 Maxa 的设备页发起控制申请。")))}</section>
    <section class="panel"><div class="panel-title"><h2>Android 设备</h2><button class="button subtle" data-action="phones"${disabled('phones')}>刷新设备</button></div>${(phones.length ? phones.map(phone => html `<div class="phone-row"><div class="phone-icon">▯</div><div class="grow"><strong>${escape(phone.name)}</strong><p class="muted">${escape(phone.activeTransport || t("当前没有连接通道"))}</p>${(phone.reason ? `<p class="inline-error">${escape(phone.reason)}</p>` : '')}<code>${escape(phone.id)}</code><button class="button subtle copy-phone" data-action="copy-phone" data-id="${escape(phone.id)}">复制设备 ID</button></div><div class="phone-controls">${badge(phone.state)}${(phone.removed ? t("<span class=\"tag paused\">已移除</span>") : '')}<button class="button ${(phone.removed ? 'primary' : 'danger')}" data-action="${(phone.removed ? 'phone-add' : 'phone-stop')}" data-id="${escape(phone.id)}"${disabled(`phone:${phone.id}`)}>${(phone.removed ? t("重新添加") : t("停止当前操作"))}</button>${(phone.removed ? '' : html `<button class="button subtle danger-text" data-action="phone-remove" data-id="${escape(phone.id)}"${disabled(`phone:${phone.id}`)}>移除设备</button>`)}</div></div>`).join('') : empty(t("没有已添加的 Android"), t("开启 Android 模块后，通过 USB 授权或无线配对添加设备。")))}${(removedPhones.length ? html `<details><summary>已移除设备 (${removedPhones.length})</summary><p class="hint">这些记录仅用于阻止自动重新添加，不表示设备仍可访问。</p>${removedPhones.map(phone => html `<div class="list-row"><div><strong>${escape(phone.name)}</strong><p class="muted">已移除 · 不会自动恢复</p><code>${escape(short(phone.id))}</code></div><button class="button" data-action="phone-add" data-id="${escape(phone.id)}"${disabled(`phone:${phone.id}`)}>重新添加</button></div>`).join('')}</details>` : '')}<div class="phone-connect-actions"><button class="button" data-action="phone-pair">无线配对</button><button class="button subtle" data-action="phone-connect">连接已配对手机</button></div><p class="hint">新请求自动尝试恢复连接，无需续租或手动恢复控制。移除的设备不会自动加回；手机调试授权及配对码仍由本人在本地处理。</p></section>`;
}
function activity() {
    const controls = snapshot?.controls ?? [];
    const jobs = snapshot?.jobs ?? [];
    return html `<div class="page-heading"><div class="eyebrow">ACTIVITY</div><h1>每次操作都有归属</h1><p>断开连接不代表命令已经停止。以远端任务的实际状态为准。</p></div>
    <section class="panel"><div class="panel-title"><h2>正在进行的控制</h2><span class="muted">${controls.length} 个操作 · 无租期</span></div>${(controls.length ? controls.map(control => html `<div class="list-row"><div><strong>${(control.module === 'windows' ? t("Windows 桌面") : 'Android')} · ${escape(control.resource)}</strong><p class="muted">${escape(control.operation)} · ${time(control.startedAt)}</p><p class="muted">客户端 ${escape(short(control.clientId))} · 会话 ${escape(short(control.chatId))}</p></div><button class="button danger" data-action="stop-client" data-id="${escape(control.clientId)}">停止当前操作</button></div>`).join('') : empty(t("当前没有控制操作"), t("配对保持有效；实际操作进行时在这里显示其归属。")))}</section>
    <section class="panel"><div class="panel-title"><h2>远程任务</h2><span class="muted">不展示命令正文或凭据</span></div>${(jobs.length ? html `<div class="table-wrap"><table><thead><tr><th>任务</th><th>客户端 / 会话</th><th>状态</th><th>开始时间</th></tr></thead><tbody>${jobs.map(job => `<tr><td><code title="${escape(job.id)}">${escape(short(job.id))}</code>${job.exitCode !== null ? html `<small>退出码 ${job.exitCode}</small>` : ''}</td><td><code>${escape(short(job.clientId))}</code><small>${escape(short(job.chatId))}</small></td><td>${badge(job.state)}</td><td class="muted">${time(job.createdAt)}</td></tr>`).join('')}</tbody></table></div>` : empty(t("没有任务记录"), t("这里显示受管命令，不把应用启动确认当成业务完成。")))}</section>`;
}
function settingsPage() {
    if (!settings)
        return empty(t("设置暂不可用"), t("请刷新状态后重试。"));
    const a = settings.appearance;
    const option = (value: string, label: string, current: string) => `<option value="${value}"${value === current ? ' selected' : ''}>${label}</option>`;
    return html `<div class="page-heading"><div class="eyebrow">PREFERENCES</div><h1>设置</h1><p>更改自动保存到本机，下次启动继续使用。</p></div>
    <section class="panel"><fieldset class="settings-fields"${(busy.has('settings') ? ' disabled' : '')}>
    <label class="setting-row"><span><b>随 Windows 登录启动</b><small>仅为当前用户设置，不需要管理员权限。</small></span><input type="checkbox" data-setting="autostart"${(settings.autostart ? ' checked' : '')}></label>
    <label class="setting-row"><span><b>允许复制符号链接</b><small>${escape(settings.copySymlinksError ?? t('需要 Windows 开发者模式；只有验证通过后才会开启。'))}</small></span><input type="checkbox" data-setting="copySymlinks"${(settings.copySymlinks ? ' checked' : '')}></label>
    ${settings.copySymlinksError ? html`<div class="setting-row"><span></span><button class="button" data-action="developer-settings">打开 Windows 开发者设置</button></div>` : ''}
    <label class="setting-row"><span><b>启动时最小化到系统托盘</b><small>后台恢复模块；通过右下角托盘打开窗口。</small></span><input type="checkbox" data-setting="startMinimized"${(a.startMinimized ? ' checked' : '')}></label>
    <label class="setting-row"><span><b>主题</b></span><select data-setting="theme">${option('system', t('跟随系统'), a.theme)}${option('light', t('浅色'), a.theme)}${option('dark', t('深色'), a.theme)}</select></label>
    <label class="setting-row"><span><b>语言</b><small>不支持的系统语言使用英文。</small></span><select data-setting="language">${option('system', t('跟随系统'), a.language)}${option('zh-CN', t("简体中文"), a.language)}${option('en', 'English', a.language)}</select></label>
    </fieldset></section>`;
}
async function refreshSettings() {
    if (busy.has('settings')) return;
    render();
    try {
        settings = await command('device_settings', {});
        applyPreferences();
    } catch (error) {
        failure = String(error);
    }
    render();
}
window.addEventListener('focus', () => { if (page === 'settings') handle(refreshSettings()); });
function render() {
    const focused = document.activeElement instanceof HTMLInputElement ? document.activeElement : null;
    const selection = focused ? { id: focused.id, start: focused.selectionStart, end: focused.selectionEnd } : null;
    app.innerHTML = html `<aside class="sidebar"><a class="brand" href="#" data-page="overview"><span class="brand-mark">M</span><span>Maxa <b>Device</b><small>设备连接与控制</small></span></a><nav aria-label="主导航">${(Object.keys(pages) as Page[]).map(item => `<button class="nav-item ${page === item ? 'selected' : ''}" data-page="${item}"${page === item ? ' aria-current="page"' : ''}><span aria-hidden="true">${icons[item]}</span>${t(pages[item])}${item === 'devices' && snapshot?.pairing.length ? `<i class="nav-dot"></i>` : ''}</button>`).join('')}</nav><div class="sidebar-bottom"><span class="local-label">本机身份</span><strong>${escape(snapshot?.identity.displayName ?? t("等待服务"))}</strong><code title="${escape(snapshot?.identity.hostId ?? '')}">${escape(snapshot ? short(snapshot.identity.hostId) : t("尚未连接后端"))}</code><div>${badge(snapshot?.service.state ?? 'starting')}</div><button class="exit-button" data-action="exit">退出 App</button></div></aside>
    <div class="workspace"><header class="toolbar"><span>${t(pages[page])}</span><div><button class="icon-button" title="刷新状态" aria-label="刷新状态" data-action="refresh"${disabled('refresh')}>↻</button><button class="button danger" data-action="stop"${disabled('stop')}${(snapshot ? '' : ' disabled')}><span aria-hidden="true">■</span> 停止当前操作</button></div></header><main>${(failure || snapshot?.service.error ? html `<div class="error-banner" role="alert"><strong>操作未完成</strong><p>${escape(failure || snapshot?.service.error)}</p></div>` : '')}${(booting ? t("<div class=\"loading\"><span class=\"spinner\"></span> 正在连接本机服务…</div>") : !snapshot ? html `<section class="panel">${empty(t("尚未连接到本机服务"), t("请从 Maxa Device 原生应用打开。连接错误不会显示成可用状态。"))}<button class="button" data-action="refresh">重新连接</button></section>` : ({ overview, modules, devices, activity, settings: settingsPage })[page]())}</main></div>`;
    if (selection?.id) {
        const next = document.getElementById(selection.id);
        if (next instanceof HTMLInputElement) {
            next.focus({ preventScroll: true });
            try {
                next.setSelectionRange(selection.start, selection.end);
            }
            catch { /* URL inputs have no selection API. */ }
        }
    }
}
function dialog(title: string, body: string, submitLabel: string, onSubmit: (form: HTMLFormElement) => Promise<void>, danger = false) {
    dialogRoot.innerHTML = html `<dialog aria-labelledby="dialog-title"><form id="dialog-form"><div class="dialog-heading"><h2 id="dialog-title">${escape(title)}</h2><button type="button" class="icon-button" data-close aria-label="关闭">×</button></div>${body}<p class="dialog-error" role="alert"></p><div class="dialog-actions"><button type="button" class="button subtle" data-close>取消</button><button class="button ${(danger ? 'danger' : 'primary')}" type="submit">${escape(submitLabel)}</button></div></form></dialog>`;
    const element = dialogRoot.querySelector('dialog')!;
    const form = dialogRoot.querySelector('form')!;
    let submitting = false;
    element.addEventListener('cancel', event => {
        if (submitting)
            event.preventDefault();
    });
    element.addEventListener('close', () => element.remove());
    element.querySelectorAll('[data-close]').forEach(button => button.addEventListener('click', () => element.close()));
    form.addEventListener('submit', event => {
        event.preventDefault();
        if (submitting)
            return;
        submitting = true;
        const buttons = Array.from(form.querySelectorAll<HTMLButtonElement>('button'));
        buttons.forEach(button => { button.disabled = true; });
        void onSubmit(form).then(() => element.close()).catch(error => { form.querySelector('.dialog-error')!.textContent = String(error); }).finally(() => { submitting = false; buttons.forEach(button => { button.disabled = false; }); });
    });
    element.showModal();
}
function review(request: PairRequest) {
    const body = html `<p><strong>${escape(request.clientName)}</strong> 请求控制这台设备。</p><p class="hint">请与发起请求的 Maxa 设备页逐字核对。名称相同不代表身份相同；不一致时拒绝。</p><div class="panel"><span class="muted">核对码</span><h2><code>${escape(request.verificationCode)}</code></h2><p class="muted">客户端身份</p><code>${escape(request.clientIdentity)}</code></div><p>批准后，该客户端可长期使用 App 当前和以后开启的所有模块，包括相应文件、命令、桌面和手机操作。没有目录沙箱或逐项权限表；关闭模块、移除手机或解除配对可持续禁止访问，停止只取消当前操作。</p><button class="button subtle danger-text" type="button" id="reject-pair">拒绝请求</button>`;
    dialog(t("核对并批准控制"), body, t("核对一致，允许控制"), async () => {
        await operate(`pair:${request.id}`, 'device_pair_decide', { requestId: request.id, accept: true });
        announce(t("已批准控制申请，配对长期有效，直到你主动解除"));
    });
    dialogRoot.querySelector('#reject-pair')?.addEventListener('click', event => {
        const button = event.currentTarget as HTMLButtonElement;
        button.disabled = true;
        handle(operate(`pair:${request.id}`, 'device_pair_decide', { requestId: request.id, accept: false })
            .then(() => dialogRoot.querySelector('dialog')?.close()).finally(() => { button.disabled = false; }));
    });
}
app.addEventListener('click', event => {
    if (!(event.target instanceof Element))
        return;
    const button = event.target.closest<HTMLElement>('[data-page],[data-action]');
    if (!button || button.hasAttribute('disabled'))
        return;
    event.preventDefault();
    if (button.dataset.page && button.dataset.page in pages) {
        page = button.dataset.page as Page;
        if (page === 'settings') handle(refreshSettings());
        else render();
        return;
    }
    const id = button.dataset.id ?? '';
    switch (button.dataset.action) {
        case 'developer-settings':
            handle(operate('developer-settings', 'device_developer_settings', {}));
            break;
        case 'refresh':
            handle((async () => { settings = await command('device_settings', {}); applyPreferences(); await operate('refresh', 'device_snapshot', {}); })());
            break;
        case 'phones':
            handle(operate('phones', 'device_phone_refresh', {}));
            break;
        case 'copy-phone':
            handle(navigator.clipboard.writeText(id).then(() => announce(t("设备 ID 已复制"))).catch(error => { failure = String(error); render(); throw error; }));
            break;
        case 'phone-stop':
            handle(operate(`phone:${id}`, 'device_stop_control', { phoneId: id }).then(() => announce(t("已请求停止这台手机当前操作；新请求可继续。持续禁用请移除设备或关闭模块。"))));
            break;
        case 'phone-remove':
            dialog(t("移除这台 Android"), t("<p>立即停止这台手机的控制并禁止后续访问。刷新、重连不会自动恢复；需要你明确重新添加。不会操作其他手机或清理手机数据。</p>"), t("移除设备"), async () => {
                await operate(`phone:${id}`, 'device_phone_remove', { phoneId: id });
                announce(t("设备已移除，自动发现不会重新开放控制"));
            }, true);
            break;
        case 'phone-add':
            dialog(t("重新添加这台 Android"), t("<p>重新核对这台手机的当前连接通道。添加后，已配对客户端可通过开启的 Android 模块使用它；仍需满足手机系统调试权限。</p>"), t("重新添加"), async () => {
                await operate(`phone:${id}`, 'device_phone_add', { phoneId: id });
                announce(t("已重新添加，请以设备实际连接状态为准"));
            });
            break;
        case 'phone-pair':
            dialog(t("Android 无线配对"), t("<p>在手机“无线调试 → 使用配对码配对设备”中取得以下信息。配对码不会写入日志或发送给 Agent。</p><label for=\"phone-pair-address\">IP 与配对端口</label><input id=\"phone-pair-address\" name=\"address\" placeholder=\"IP:配对端口\" required autocomplete=\"off\"><label for=\"phone-pair-code\">六位配对码</label><input id=\"phone-pair-code\" name=\"code\" type=\"password\" inputmode=\"numeric\" pattern=\"[0-9]{6}\" maxlength=\"6\" required autocomplete=\"off\">"), t("确认配对"), async (form) => {
                const values = new FormData(form);
                await operate('phone-pair', 'device_phone_pair', { address: String(values.get('address') ?? '').trim(), pairCode: String(values.get('code') ?? '') });
                announce(t("配对已完成；如设备未自动出现，请连接无线调试主页显示的地址"));
            });
            break;
        case 'phone-connect':
            dialog(t("连接已配对 Android"), t("<p>填写手机“无线调试”主页上的地址和端口，不是配对码窗口的端口。重启或重新开启调试后，端口可能变化。</p><label for=\"phone-connect-address\">IP 与连接端口</label><input id=\"phone-connect-address\" name=\"address\" placeholder=\"IP:连接端口\" required autocomplete=\"off\">"), t("连接设备"), async (form) => {
                await operate('phone-connect', 'device_phone_connect', { address: String(new FormData(form).get('address') ?? '').trim() });
                announce(t("连接请求已完成，请确认设备的实际可用状态"));
            });
            break;
        case 'stop':
            handle(operate('stop', 'device_stop_control', {}).then(() => announce(t("已提交当前操作的停止请求；新请求仍可继续，持续禁用请关闭模块或解除配对。"))));
            break;
        case 'stop-client':
            handle(operate('stop', 'device_stop_control', { clientId: id }));
            break;
        case 'module-retry':
            handle(operate(`module:${id}`, 'device_module', { module: id as ModuleId, enabled: true }));
            break;
        case 'module': {
            const module = id as ModuleId;
            const current = snapshot?.modules.find(item => item.id === module);
            const enabling = !current?.enabled;
            dialog(`${enabling ? t("开启能力") : t("关闭能力")} · ${t(moduleCopy[module].title)}`, `<p>${enabling ? t("App 将检查并按需安装固定版本私有依赖，不修改全局环境。开启后，所有已配对客户端都可使用此能力，不再逐项授权；移除的手机不会因此重新添加。") : t("将拒绝新调用并取消此模块的受管工作。已经发生的操作不会回滚。")}</p>`, enabling ? t("开启能力") : t("关闭能力"), async () => { await operate(`module:${module}`, 'device_module', { module, enabled: enabling }); }, !enabling);
            break;
        }
        case 'review': {
            const request = snapshot?.pairing.find(item => item.id === id);
            if (request)
                review(request);
            break;
        }
        case 'revoke':
            dialog(t("撤销客户端授权"), t("<p>立即拒绝该客户端的新调用，释放其控制权并取消受管工作。已发生的副作用不会回滚，重新连接需要再次配对。</p>"), t("撤销授权"), async () => { await operate(`revoke:${id}`, 'device_client_revoke', { clientId: id }); }, true);
            break;
        case 'exit':
            dialog(t("退出 Maxa Device"), t("<p>将停止受管模块、设备控制和受管进程。已成功移交的独立应用不会被自动关闭。若只想隐藏窗口，请关闭到托盘。</p>"), t("退出 App"), async () => { await command('device_exit', {}); }, true);
            break;
    }
});
app.addEventListener('change', event => {
    const input = event.target;
    if (!(input instanceof HTMLInputElement || input instanceof HTMLSelectElement) || !input.dataset.setting || !settings || busy.has('settings'))
        return;
    const field = input.dataset.setting;
    const appearance = { ...settings.appearance };
    if (field === 'startMinimized' && input instanceof HTMLInputElement)
        appearance.startMinimized = input.checked;
    if (field === 'theme')
        appearance.theme = input.value as Appearance['theme'];
    if (field === 'language')
        appearance.language = input.value as Appearance['language'];
    handle((async () => {
        try {
            settings = field === 'autostart' && input instanceof HTMLInputElement
                ? await operate('settings', 'device_autostart', { enabled: input.checked })
                : field === 'copySymlinks' && input instanceof HTMLInputElement
                    ? await operate('settings', 'device_copy_symlinks', { enabled: input.checked })
                    : await operate('settings', 'device_settings_save', { appearance });
            applyPreferences();
            render();
            announce(t('设置已保存'));
        }
        catch {
            try {
                settings = await command('device_settings', {});
                applyPreferences();
            }
            catch { /* retain visible save failure */ }
            render();
        }
    })());
});
render();
void (async () => {
    try {
        await listen('maxa-device:open-settings', () => { page = 'settings'; handle(refreshSettings()); });
        await listen<DeviceSnapshot>(snapshotEvent, event => { snapshot = event.payload; render(); });
        snapshot = await command('device_snapshot', {});
        settings = await command('device_settings', {});
        applyPreferences();
    }
    catch (error) {
        failure = String(error);
    }
    finally {
        booting = false;
        render();
    }
})();
