// Local loading page: polls the shell for the startup phase so nothing
// depends on event timing. The phase also drives the window title (visible
// in the taskbar). The Node runtime download renders as a determinate
// progress bar; the npm install renders a live readout (bytes downloaded,
// network speed, installed package count); other phases show an
// indeterminate spinner with an elapsed counter. On failure the reason, the
// host's recent stderr, and the shell log path stay on screen.
const statusEl = document.getElementById('status')
const detailEl = document.getElementById('detail')
const hintEl = document.getElementById('hint')
const spinnerEl = document.getElementById('spinner')
const progressEl = document.getElementById('progress')
const progressFill = document.getElementById('progress-fill')
const progressPct = document.getElementById('progress-pct')

// The runtime-download notice belongs to the bootstrap path only. Left on
// screen it reads as "a 350 MB download is under way" on every launch,
// including the ones where an existing dsh was found and nothing is being
// downloaded at all.
const BOOTSTRAP_PHASES = [
  '未检测到 dsh',
  '正在下载 Node.js',
  '正在解压 Node.js',
  '正在安装 dsh',
]

let installStartedAt = null

function formatElapsed(totalSeconds) {
  const minutes = Math.floor(totalSeconds / 60)
  const seconds = totalSeconds % 60
  return minutes > 0 ? `${minutes} 分 ${seconds} 秒` : `${seconds} 秒`
}

function formatSpeed(bytesPerSecond) {
  if (bytesPerSecond >= 1048576) {
    return (bytesPerSecond / 1048576).toFixed(1) + ' MB/s'
  }
  return Math.max(0, Math.round(bytesPerSecond / 1024)) + ' KB/s'
}

function npmReadout(npm, elapsedSeconds) {
  const parts = [npm.phase]
  if (npm.phase === '正在下载依赖包…' || npm.phase === '正在获取包元数据…') {
    parts.push(`已下载 ${Math.round(npm.bytes / 1048576)} MB`)
  }
  if (npm.phase === '正在下载依赖包…') {
    parts.push(`网速 ${formatSpeed(npm.speed)}`)
  }
  if (npm.phase === '正在解压安装…') {
    parts.push(`已安装 ${npm.packages} 个包`)
  }
  parts.push(`已用时 ${formatElapsed(elapsedSeconds)}`)
  return parts.join(' · ')
}

async function poll() {
  const status = await window.__TAURI__.core.invoke('startup_status')
  if (status.error) {
    statusEl.textContent = status.error
    spinnerEl.hidden = true
    progressEl.hidden = true
    hintEl.hidden = true
    const parts = []
    if (status.stderrTail && status.stderrTail.trim().length > 0) {
      parts.push('宿主输出:\n' + status.stderrTail.trim())
    }
    parts.push('完整日志: ' + status.logPath)
    detailEl.textContent = parts.join('\n\n')
    detailEl.hidden = false
    return
  }
  detailEl.hidden = true
  hintEl.hidden = !BOOTSTRAP_PHASES.some((prefix) => status.phase.startsWith(prefix))

  let phase = status.phase
  if (phase.startsWith('正在安装')) {
    if (installStartedAt === null) {
      installStartedAt = Date.now()
    }
    const elapsed = Math.floor((Date.now() - installStartedAt) / 1000)
    if (status.npmProgress) {
      phase += `（${npmReadout(status.npmProgress, elapsed)}）`
    } else if (elapsed >= 5) {
      phase += `（已用时 ${formatElapsed(elapsed)}）`
    }
  } else {
    installStartedAt = null
  }
  statusEl.textContent = phase

  if (status.progress && status.progress.total > 0) {
    progressEl.hidden = false
    spinnerEl.hidden = true
    const percent = Math.min(100, Math.round((status.progress.done * 100) / status.progress.total))
    progressFill.style.width = percent + '%'
    progressPct.textContent = percent + '%'
  } else {
    progressEl.hidden = true
    spinnerEl.hidden = false
  }

  setTimeout(poll, 300)
}

poll().catch(() => {
  statusEl.textContent = '启动失败:无法连接宿主进程'
  spinnerEl.hidden = true
  progressEl.hidden = true
})
