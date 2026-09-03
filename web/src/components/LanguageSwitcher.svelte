<script lang="ts">
  import { locale, setLocale, t } from '../lib/i18n'
</script>

<div class="language-switcher" role="group" aria-label={$t('Language')}>
  <button type="button" aria-pressed={$locale === 'en'} onclick={() => setLocale('en')}>{$t('English')}</button>
  <button type="button" aria-pressed={$locale === 'zh-CN'} onclick={() => setLocale('zh-CN')}>{$t('Simplified Chinese')}</button>
</div>
