// Share instance detection across pages; temporary connection failures are expected.
export function createDaemonMonitor({
  fetchHealth = () => fetch('/api/v1/health', { cache: 'no-store', signal: AbortSignal.timeout(5000) }),
  reload = () => location.reload(),
  onHealth = () => {},
} = {}) {
  let startedAt = null;
  let reloading = false;
  let checking = false;
  function observe(value) {
    if (!Number.isFinite(value)) return;
    if (startedAt === null) startedAt = value;
    else if (value !== startedAt && !reloading) {
      reloading = true;
      reload();
    }
  }
  async function check() {
    if (checking || reloading) return;
    checking = true;
    try {
      const response = await fetchHealth();
      if (!response.ok) return;
      const health = await response.json();
      observe(health.started_at_ms);
      onHealth(health);
    } catch {
      // Keep the known instance until the service responds again.
    } finally {
      checking = false;
    }
  }
  function start() {
    void check();
    const timer = setInterval(check, 2000);
    const resume = () => { if (!document.hidden) void check(); };
    document.addEventListener('visibilitychange', resume);
    window.addEventListener('pageshow', resume);
    return () => {
      clearInterval(timer);
      document.removeEventListener('visibilitychange', resume);
      window.removeEventListener('pageshow', resume);
    };
  }
  return { observe, check, start };
}
