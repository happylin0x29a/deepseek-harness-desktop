// Local loading page: polls the shell for the startup phase so nothing
// depends on event timing. The phase also drives the window title (visible
// in the taskbar). On failure the reason, the host's recent stderr, and the
// shell log path stay on screen.
const statusEl = document.getElementById('status')
const detailEl = document.getElementById('detail')
const spinnerEl = document.getElementById('spinner')

async function poll() {
  const status = await window.__TAURI__.core.invoke('startup_status')
  statusEl.textContent = status.error ?? status.phase
  if (status.error) {
    spinnerEl.style.display = 'none'
    const parts = []
    if (status.stderrTail && status.stderrTail.trim().length > 0) {
      parts.push('宿主输出:\n' + status.stderrTail.trim())
    }
    parts.push('完整日志: ' + status.logPath)
    detailEl.textContent = parts.join('\n\n')
    return
  }
  setTimeout(poll, 400)
}

poll().catch(() => {
  statusEl.textContent = '启动失败:无法连接宿主进程'
  spinnerEl.style.display = 'none'
})
