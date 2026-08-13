// Local loading page: shows startup progress while the Rust host spawns
// `dsh desktop` and waits for its readiness line. State arrives as
// `startup-state` events; on `ready` the host resizes the window and
// navigates it to the surface, on `failed` the reason stays on screen.
const statusEl = document.getElementById('status')

async function main() {
  const { listen } = window.__TAURI__.event
  await listen('startup-state', (event) => {
    const state = event.payload
    if (typeof state !== 'string') return
    statusEl.textContent = state
  })
}

main().catch(() => {
  statusEl.textContent = '启动失败:无法连接宿主进程'
})
