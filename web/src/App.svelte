<svelte:head>
  <title>SoloDock</title>
</svelte:head>

<main>
  <section class="hero" aria-labelledby="title">
    <div class="mark" aria-hidden="true">SD</div>
    <p class="eyebrow">Single-host container deployment</p>
    <h1 id="title">SoloDock</h1>
    <p class="summary">
      A lightweight console for deploying prebuilt container images by immutable digest.
    </p>

    <div class="status" role="status">
      <span class="status-dot"></span>
      <div>
        <strong>Bootstrap ready</strong>
        <span>The application scaffold is running. Product features are under development.</span>
      </div>
    </div>

    <p class="endpoint"><code>GET /healthz</code> reports API availability.</p>
  </section>
</main>
