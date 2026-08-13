// Local loading page: shows startup progress while the Rust host spawns
// `dsh desktop` and waits for its readiness line. State arrives as
// `startup-state` events; the recorded error is also asked once on load so
// an early failure is never missed. On `ready` the host grows the window and
// navigates it onto the surface.
const statusEl = document.getElementById('status')
const spinnerEl = document.getElementById('spinner')

async function main() {
  const { invoke } = window.__TAURI__.core
  const { listen } = window.__TAURI__.event
  // Ask first: a failure recorded before this script ran must still show.
  const recorded = await invoke('startup_error')
  if (typeof recorded === 'string' && recorded.length > 0) {
    show(recorded)
  }
  await listen('startup-state', (event) => {
    const state = event.payload
    if (typeof state !== 'string') return
    if (state.startsWith('启动失败')) show(state)
    else statusEl.textContent = state
  })
}

function show(message) {
  statusEl.textContent = message
  spinnerEl.style.display = 'none'
}

main().catch(() => {
  show('启动失败:无法连接宿主进程')
})
