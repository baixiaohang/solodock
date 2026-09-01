<script lang="ts">
  import { bootstrap, login } from '../lib/auth'
  import { ApiError } from '../lib/api'

  let { mode }: { mode: 'setup' | 'login' } = $props()
  let token = $state('')
  let password = $state('')
  let confirm = $state('')
  let busy = $state(false)
  let error = $state('')

  async function submit() {
    error = ''
    if (mode === 'setup' && password !== confirm) { error = '两次输入的密码不一致'; return }
    const passwordError = validatePassword(password)
    if (passwordError) { error = passwordError; return }
    busy = true
    try {
      if (mode === 'setup') await bootstrap(token, password)
      else await login(password)
      token = ''; password = ''; confirm = ''
    } catch (cause) {
      error = cause instanceof ApiError ? authMessage(cause.body.code) : '连接控制面失败，请稍后重试'
    } finally { busy = false }
  }

  function validatePassword(value: string): string {
    const trimmed = value.trim()
    const scalarCount = Array.from(trimmed).length
    const byteCount = new TextEncoder().encode(trimmed).length
    return scalarCount >= 14 && scalarCount <= 128 && byteCount <= 512
      ? ''
      : '密码需为 14–128 个 Unicode 字符（首尾空白不计）'
  }

  function authMessage(code: string): string {
    if (code === 'BOOTSTRAP_TOKEN_INVALID') return 'Bootstrap token 无效'
    if (code === 'AUTH_INVALID') return '密码不正确'
    if (code === 'AUTH_COOLDOWN') return '登录尝试过多，请稍后重试'
    return '认证请求失败'
  }
</script>

<main class="auth-shell">
  <section class="auth-card">
    <div class="logo">SD</div>
    <p class="eyebrow">SOLODOCK CONTROL PLANE</p>
    <h1>{mode === 'setup' ? '初始化管理员' : '欢迎回来'}</h1>
    <p class="muted">{mode === 'setup' ? '输入服务器 bootstrap.token 文件中的一次性 token，并设置管理员密码。' : '以单管理员身份进入只读 Docker 观察台。'}</p>
    <form onsubmit={(event) => { event.preventDefault(); void submit() }}>
      {#if mode === 'setup'}
        <label>Bootstrap token<input type="password" bind:value={token} required autocomplete="off" spellcheck="false" /></label>
      {/if}
      <label>密码（14–128 个字符）<input type="password" bind:value={password} required minlength="14" autocomplete={mode === 'login' ? 'current-password' : 'new-password'} /></label>
      {#if mode === 'setup'}
        <label>确认密码<input type="password" bind:value={confirm} required minlength="14" autocomplete="new-password" /></label>
      {/if}
      {#if error}<p class="form-error" role="alert">{error}</p>{/if}
      <button class="primary" disabled={busy}>{busy ? '处理中…' : mode === 'setup' ? '完成初始化' : '登录'}</button>
    </form>
    <p class="security-note">凭据仅提交到当前 origin，不会写入浏览器存储。</p>
  </section>
</main>
