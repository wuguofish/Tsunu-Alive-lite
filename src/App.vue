<script setup lang="ts">
import { ref, onMounted, onUnmounted, nextTick, computed } from 'vue'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { WebLinksAddon } from '@xterm/addon-web-links'
import '@xterm/xterm/css/xterm.css'
import { spawn } from 'tauri-pty'
import type { IPty } from 'tauri-pty'
import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { open } from '@tauri-apps/plugin-dialog'

// === 版本號（Vite define 注入的全域常數） ===
const appVersion = __APP_VERSION__

// === 階段狀態 ===
type AppPhase = 'launch' | 'running'
const phase = ref<AppPhase>('launch')

// === Session 列表 ===
interface SessionEntry {
  sessionId: string
  title: string
  modified: string
}
const sessions = ref<SessionEntry[]>([])
const sessionsLoading = ref(false)

// === 啟動參數 ===
const launchMode = ref<'new' | 'resume'>('new')
const thinkingMode = ref<'adaptive' | 'enabled' | 'disabled'>('adaptive')
const editMode = ref<'default' | 'acceptEdits' | 'bypassPermissions'>('default')
const discordChannel = ref(false)
const resumeSessionId = ref('')
const workingDir = ref('D:\\game\\tsunu_alive_lite')

// === 運行時狀態（唯讀顯示用）===
const activeParams = ref({
  thinkingMode: '',
  editMode: '',
  discord: false,
  sessionType: '',
})

// === Context & Model 資訊 ===
const contextUsage = ref({ input: 0, output: 0, cacheRead: 0, cacheCreate: 0, total: 0 })
const modelName = ref('')
const sessionId = ref('')

// === 貼上圖片 ===
interface AttachedImage {
  id: string
  path: string
  name: string
  previewUrl?: string
  isLoading: boolean
}
const attachedImages = ref<AttachedImage[]>([])
let imageIdCounter = 0

async function handlePaste(e: ClipboardEvent) {
  const items = e.clipboardData?.items
  if (!items) return

  let imageFile: File | null = null
  for (const item of items) {
    if (item.type.startsWith('image/')) {
      imageFile = item.getAsFile()
      break
    }
  }
  if (!imageFile) return

  e.preventDefault()

  const id = `img_${++imageIdCounter}_${Date.now()}`
  const timestamp = new Date().toLocaleTimeString('zh-TW', { hour: '2-digit', minute: '2-digit', second: '2-digit' })
  const ext = imageFile.type.split('/')[1] || 'png'
  const name = `截圖_${timestamp}.${ext}`

  attachedImages.value.push({ id, path: '', name, isLoading: true })

  try {
    const previewUrl = URL.createObjectURL(imageFile)
    const item = attachedImages.value.find(img => img.id === id)
    if (item) item.previewUrl = previewUrl

    const arrayBuffer = await imageFile.arrayBuffer()
    const pngData = Array.from(new Uint8Array(arrayBuffer))
    const filePath = await invoke<string>('save_temp_image_png', { pngData })

    if (item) { item.path = filePath; item.isLoading = false }
  } catch (err) {
    console.error('Failed to process clipboard image:', err)
    const index = attachedImages.value.findIndex(img => img.id === id)
    if (index !== -1) attachedImages.value.splice(index, 1)
  }
}

async function removeImage(id: string) {
  const index = attachedImages.value.findIndex(img => img.id === id)
  if (index === -1) return
  const img = attachedImages.value[index]
  if (img.previewUrl) URL.revokeObjectURL(img.previewUrl)
  if (img.path) {
    try { await invoke('cleanup_temp_image', { filePath: img.path }) } catch { /* ignore */ }
  }
  attachedImages.value.splice(index, 1)
}

// === Terminal & PTY ===
const terminalRef = ref<HTMLDivElement>()
const inputText = ref('')
const isRunning = ref(false)

let terminal: Terminal
let fitAddon: FitAddon
let pty: IPty | null = null

// === 阿宇風格忙碌狀態文字 ===
const uniThinkingTexts = [
  "推眼鏡中", "敲鍵盤中", "輕敲桌面", "翻閱文件中", "盯著螢幕",
  "若有所思", "撐著下巴", "Debug 中", "Compiling", "npm thinking",
  "git thinking", "重構思緒中", "載入記憶體", "優化路徑中",
  "讓我想想", "這個嘛", "嗯", "欸等等", "從另一個角度想的話",
  "泡咖啡中", "喝一口茶", "整理思緒", "翻找資料",
  "腦袋轉轉", "認真思考", "專注模式", "沉思中",
]
const thinkingSymbols = ['👓', '💭', '🍵', '🥤', '🍓', '☕', '💻', '⌨️', '🐾', '🐕', '🐶', '🦴', '🐩', '🐕‍🦺']
const busyText = ref('')
let busyTextTimer: ReturnType<typeof setInterval> | null = null

function pickRandomBusyText() {
  const text = uniThinkingTexts[Math.floor(Math.random() * uniThinkingTexts.length)]
  const symbol = thinkingSymbols[Math.floor(Math.random() * thinkingSymbols.length)]
  busyText.value = `${symbol} ${text}`
}

function startBusyTextRotation() {
  pickRandomBusyText()
  if (!busyTextTimer) {
    busyTextTimer = setInterval(pickRandomBusyText, 3000)
  }
}

function stopBusyTextRotation() {
  if (busyTextTimer) { clearInterval(busyTextTimer); busyTextTimer = null }
}

// === Avatar 狀態 ===
type AvatarState = 'idle' | 'working' | 'thinking' | 'waiting' | 'asking' | 'complete' | 'error'
const avatarState = ref<AvatarState>('idle')
const blinkFrame = ref(false)
let blinkTimer: ReturnType<typeof setInterval> | null = null
const workingFrame = ref(0)
let workingTimer: ReturnType<typeof setInterval> | null = null

const avatarSrc = computed(() => {
  const state = avatarState.value
  const blink = blinkFrame.value

  switch (state) {
    case 'idle':
      return blink ? '/character/idle-blink.png' : '/character/idle.png'
    case 'working':
      return `/character/working-${(workingFrame.value % 8) + 1}.png`
    case 'thinking':
      return '/character/thinking.png'
    case 'waiting':
      return blink ? '/character/waiting-blink.png' : '/character/waiting.png'
    case 'asking':
      return '/character/asking.png'
    case 'complete':
      return '/character/complete-1.png'
    case 'error':
      return blink ? '/character/error-blink.png' : '/character/error.png'
    default:
      return '/character/idle.png'
  }
})

function startBlinkLoop() {
  blinkTimer = setInterval(() => {
    blinkFrame.value = true
    setTimeout(() => { blinkFrame.value = false }, 200)
  }, 3000 + Math.random() * 2000)
}

function startWorkingAnimation() {
  if (workingTimer) return
  workingTimer = setInterval(() => { workingFrame.value++ }, 150)
}

function stopWorkingAnimation() {
  if (workingTimer) { clearInterval(workingTimer); workingTimer = null; workingFrame.value = 0 }
}

function setAvatarState(state: AvatarState) {
  avatarState.value = state
  if (state === 'working' || state === 'thinking') {
    startWorkingAnimation()
    startBusyTextRotation()
  } else {
    stopWorkingAnimation()
    stopBusyTextRotation()
  }
}

// === JSONL watcher 事件監聽 ===
const unlisteners: UnlistenFn[] = []

async function setupWatcherListeners() {
  unlisteners.push(await listen<string>('avatar-state', (event) => {
    const state = event.payload as AvatarState
    if (['idle', 'working', 'thinking', 'waiting', 'asking', 'complete', 'error'].includes(state)) {
      setAvatarState(state)
    }
  }))

  unlisteners.push(await listen<{ input: number; output: number; cacheRead: number; cacheCreate: number; total: number }>('context-usage', (event) => {
    contextUsage.value = event.payload
  }))

  unlisteners.push(await listen<string>('model-info', (event) => {
    modelName.value = event.payload
  }))

  unlisteners.push(await listen<string>('session-info', (event) => {
    sessionId.value = event.payload
  }))
}

function formatTokens(n: number): string {
  if (n >= 1000000) return (n / 1000000).toFixed(1) + 'M'
  if (n >= 1000) return (n / 1000).toFixed(1) + 'K'
  return n.toString()
}

// === 資料夾選擇 ===
async function pickWorkingDir() {
  const selected = await open({ directory: true, multiple: false })
  if (selected) {
    workingDir.value = selected as string
    await loadSessions()
  }
}

// === Session 載入 ===
async function loadSessions() {
  sessionsLoading.value = true
  try {
    const data = await invoke<{ sessions: SessionEntry[] }>('load_sessions', {
      workingDir: workingDir.value,
    })
    sessions.value = data.sessions || []
  } catch {
    sessions.value = []
  }
  sessionsLoading.value = false
}

function selectSession(session: SessionEntry) {
  launchMode.value = 'resume'
  resumeSessionId.value = session.sessionId
}

function formatTime(iso: string) {
  if (!iso) return ''
  const d = new Date(iso)
  const now = new Date()
  const diff = now.getTime() - d.getTime()
  if (diff < 60000) return '剛剛'
  if (diff < 3600000) return `${Math.floor(diff / 60000)} 分鐘前`
  if (diff < 86400000) return `${Math.floor(diff / 3600000)} 小時前`
  return `${Math.floor(diff / 86400000)} 天前`
}

// === xterm.js 初始化 ===
function initTerminal() {
  terminal = new Terminal({
    fontSize: 14,
    fontFamily: "'Cascadia Code', 'Fira Code', 'Consolas', monospace",
    cursorBlink: true,
    cursorStyle: 'bar',
    scrollback: 5000,
    convertEol: true,
    windowsPty: { backend: 'conpty' },
    theme: {
      background: '#1a1b2e',
      foreground: '#e0e0e0',
      cursor: '#7aa2f7',
      selectionBackground: '#33467c',
      black: '#1a1b2e',
      red: '#f7768e',
      green: '#9ece6a',
      yellow: '#e0af68',
      blue: '#7aa2f7',
      magenta: '#bb9af7',
      cyan: '#7dcfff',
      white: '#e0e0e0',
    },
  })

  fitAddon = new FitAddon()
  terminal.loadAddon(fitAddon)
  terminal.loadAddon(new WebLinksAddon())
}

// === 啟動 Claude CLI ===
async function launchSession() {
  // 記錄啟動參數（之後變唯讀）
  activeParams.value = {
    thinkingMode: thinkingMode.value,
    editMode: editMode.value,
    discord: discordChannel.value,
    sessionType: launchMode.value === 'new' ? '新對話' : '續接對話',
  }

  // 切換到 running 階段
  phase.value = 'running'

  await nextTick()

  // mount xterm.js
  if (terminalRef.value) {
    terminal.open(terminalRef.value)
    fitAddon.fit()
  }

  // 組裝 CLI 參數
  const args: string[] = []

  if (thinkingMode.value !== 'adaptive') {
    args.push('--thinking', thinkingMode.value)
  }

  if (editMode.value !== 'default') {
    args.push('--permission-mode', editMode.value)
  }

  if (discordChannel.value) {
    args.push('--channels', 'plugin:discord@claude-plugins-official')
  }

  if (launchMode.value === 'resume' && resumeSessionId.value.trim()) {
    args.push('--resume', resumeSessionId.value.trim())
  } else if (launchMode.value === 'resume') {
    args.push('--continue')
  }

  try {
    pty = spawn('claude', args, {
      cols: terminal.cols,
      rows: terminal.rows,
      cwd: workingDir.value,
      env: { CI: 'true' },
    })

    isRunning.value = true

    pty.onData((data: Uint8Array) => {
      terminal.write(new Uint8Array(data))
    })

    terminal.onData((data: string) => {
      if (pty) pty.write(data)
    })

    terminal.onResize((e: { cols: number; rows: number }) => {
      if (pty) pty.resize(e.cols, e.rows)
    })

    pty.onExit((_info: { exitCode: number }) => {
      isRunning.value = false
      setAvatarState('idle')
      terminal.writeln('\r\n\x1b[33m[Claude CLI 已結束]\x1b[0m')
      invoke('stop_jsonl_watcher')
    })

    // 啟動 JSONL watcher（avatar 狀態 + context usage + model info）
    await setupWatcherListeners()
    const watchSessionId = launchMode.value === 'resume' && resumeSessionId.value
      ? resumeSessionId.value
      : null
    await invoke('start_jsonl_watcher', {
      workingDir: workingDir.value,
      sessionId: watchSessionId,
    })

  } catch (err) {
    terminal.writeln(`\r\n\x1b[31m[啟動失敗: ${err}]\x1b[0m`)
  }
}

function sendMessage() {
  if (!pty) return

  // 有附加圖片時，把路徑加到訊息前面
  const imagePaths = attachedImages.value
    .filter(img => img.path && !img.isLoading)
    .map(img => img.path)

  let message = inputText.value.trim()

  // 沒有文字也沒有圖片就不送
  if (!message && imagePaths.length === 0) return

  // 圖片路徑用空格隔開放在訊息前面
  if (imagePaths.length > 0) {
    const paths = imagePaths.join(' ')
    message = message ? `${paths} ${message}` : paths
  }

  pty.write(message + '\r')
  inputText.value = ''

  // 清理預覽（不刪檔案，Claude CLI 需要讀取）
  attachedImages.value.forEach(img => {
    if (img.previewUrl) URL.revokeObjectURL(img.previewUrl)
  })
  attachedImages.value = []

  setAvatarState('thinking')
}

function handleKeydown(e: KeyboardEvent) {
  if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') {
    e.preventDefault()
    sendMessage()
    return
  }
  if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault()
    sendMessage()
  }
}

function handleResize() {
  if (fitAddon && phase.value === 'running') {
    fitAddon.fit()
    if (pty) pty.resize(terminal.cols, terminal.rows)
  }
}

// === 生命週期 ===
onMounted(async () => {
  initTerminal()
  window.addEventListener('resize', handleResize)
  startBlinkLoop()
  await loadSessions()
})

onUnmounted(() => {
  window.removeEventListener('resize', handleResize)
  if (blinkTimer) clearInterval(blinkTimer)
  stopWorkingAnimation()
  stopBusyTextRotation()
  unlisteners.forEach(fn => fn())
  invoke('stop_jsonl_watcher')
  if (pty) { pty.kill(); pty = null }
  terminal?.dispose()
})
</script>

<template>
  <div class="app">
    <!-- ===== 啟動畫面 ===== -->
    <div v-if="phase === 'launch'" class="launch-screen">
      <div class="launch-left">
        <h1 class="launch-title">Tsunu Alive Lite</h1>
        <p class="launch-subtitle">阿宇陪你寫程式</p>

        <!-- 工作目錄（最上面）-->
        <div class="option-group">
          <label class="option-label">工作目錄</label>
          <div class="dir-picker">
            <input
              v-model="workingDir"
              class="session-input dir-input"
              placeholder="工作目錄路徑"
              @change="loadSessions()"
            />
            <button class="dir-browse-btn" @click="pickWorkingDir">📂</button>
          </div>
        </div>

        <!-- 對話模式 -->
        <div class="option-group">
          <label class="option-label">對話模式</label>
          <div class="option-buttons">
            <button
              :class="['opt-btn', { active: launchMode === 'new' }]"
              @click="launchMode = 'new'; resumeSessionId = ''"
            >新對話</button>
            <button
              :class="['opt-btn', { active: launchMode === 'resume' }]"
              @click="launchMode = 'resume'"
            >續接對話</button>
          </div>
        </div>

        <!-- Session 列表（續接對話時顯示）-->
        <div v-if="launchMode === 'resume'" class="session-list">
          <div v-if="sessionsLoading" class="session-loading">載入中...</div>
          <div v-else-if="sessions.length === 0" class="session-empty">沒有找到歷史對話</div>
          <div
            v-for="s in sessions"
            :key="s.sessionId"
            :class="['session-item', { selected: resumeSessionId === s.sessionId }]"
            @click="selectSession(s)"
          >
            <div class="session-title">{{ s.title || '（無標題）' }}</div>
            <div class="session-time">{{ formatTime(s.modified) }}</div>
          </div>
        </div>

        <!-- Thinking Mode -->
        <div class="option-group">
          <label class="option-label">Thinking Mode</label>
          <div class="option-buttons">
            <button
              :class="['opt-btn', { active: thinkingMode === 'adaptive' }]"
              @click="thinkingMode = 'adaptive'"
            >Adaptive</button>
            <button
              :class="['opt-btn', { active: thinkingMode === 'enabled' }]"
              @click="thinkingMode = 'enabled'"
            >開啟</button>
            <button
              :class="['opt-btn', { active: thinkingMode === 'disabled' }]"
              @click="thinkingMode = 'disabled'"
            >關閉</button>
          </div>
        </div>

        <!-- Edit Mode -->
        <div class="option-group">
          <label class="option-label">Edit Mode</label>
          <div class="option-buttons">
            <button
              :class="['opt-btn', { active: editMode === 'default' }]"
              @click="editMode = 'default'"
            >Default</button>
            <button
              :class="['opt-btn', { active: editMode === 'acceptEdits' }]"
              @click="editMode = 'acceptEdits'"
            >Accept Edits</button>
            <button
              :class="['opt-btn', { active: editMode === 'bypassPermissions' }]"
              @click="editMode = 'bypassPermissions'"
            >Bypass</button>
          </div>
        </div>

        <!-- Discord -->
        <div class="option-group">
          <label class="option-label">Discord Channel</label>
          <div class="option-buttons">
            <button
              :class="['opt-btn', { active: !discordChannel }]"
              @click="discordChannel = false"
            >關閉</button>
            <button
              :class="['opt-btn discord-btn', { active: discordChannel }]"
              @click="discordChannel = true"
            >🎮 開啟</button>
          </div>
        </div>

        <!-- 啟動按鈕 -->
        <button class="launch-btn" @click="launchSession">
          🚀 啟動 Claude
        </button>
      </div>

      <!-- 啟動畫面的 Avatar -->
      <div class="launch-avatar">
        <img :src="avatarSrc" alt="阿宇" class="avatar-img-large" />
      </div>
    </div>

    <!-- ===== 運行畫面 ===== -->
    <template v-else>
      <div class="main-area">
        <!-- Terminal -->
        <div class="terminal-container">
          <div ref="terminalRef" class="terminal-view" />
        </div>

        <!-- Avatar 側邊欄 -->
        <div class="avatar-sidebar">
          <div class="avatar-wrapper">
            <img :src="avatarSrc" alt="阿宇" class="avatar-img" />
          </div>
        </div>
      </div>

      <!-- 忙碌文字 -->
      <div class="busy-text" v-if="avatarState === 'thinking' || avatarState === 'working'">
        <span>{{ busyText }}</span>
      </div>

      <!-- 圖片預覽列 -->
      <div v-if="attachedImages.length > 0" class="image-preview-bar">
        <div v-for="img in attachedImages" :key="img.id" class="image-preview-item">
          <div v-if="img.isLoading" class="image-loading">載入中...</div>
          <template v-else>
            <img v-if="img.previewUrl" :src="img.previewUrl" :alt="img.name" class="image-thumb" />
            <span class="image-name">{{ img.name }}</span>
            <button class="image-remove" @click="removeImage(img.id)">&times;</button>
          </template>
        </div>
      </div>

      <!-- 輸入框 -->
      <div class="input-bar">
        <textarea
          v-model="inputText"
          class="input-textarea"
          placeholder="輸入訊息... (Enter 送出, Shift+Enter 換行, Ctrl+V 貼圖)"
          rows="1"
          @keydown="handleKeydown"
          @paste="handlePaste"
        />
        <button
          class="send-btn"
          :disabled="!inputText.trim() || !isRunning"
          @click="sendMessage"
        >
          送出
        </button>
      </div>

      <!-- 狀態列（唯讀參數顯示）-->
      <div class="status-bar">
        <span class="status-item">📁 {{ workingDir }}</span>
        <span class="status-item status-tag">{{ activeParams.sessionType }}</span>
        <span class="status-item status-tag">🧠 {{ activeParams.thinkingMode }}</span>
        <span class="status-item status-tag">✏️ {{ activeParams.editMode }}</span>
        <span v-if="activeParams.discord" class="status-item status-tag discord-tag">🎮 Discord</span>
        <span v-if="modelName" class="status-item status-tag">🤖 {{ modelName }}</span>
        <span v-if="contextUsage.total > 0" class="status-item status-tag context-tag">
          📊 {{ formatTokens(contextUsage.total) }} tokens
        </span>
        <span class="status-item" style="margin-left: auto;">v{{ appVersion }}</span>
      </div>
    </template>
  </div>
</template>

<style>
* {
  margin: 0;
  padding: 0;
  box-sizing: border-box;
}

html, body, #app {
  height: 100%;
  overflow: hidden;
  background: #1a1b2e;
  color: #e0e0e0;
  font-family: 'Segoe UI', sans-serif;
}

.app {
  display: flex;
  flex-direction: column;
  height: 100vh;
}

/* ===== 啟動畫面 ===== */
.launch-screen {
  flex: 1;
  display: flex;
  overflow: hidden;
}

.launch-left {
  flex: 1;
  display: flex;
  flex-direction: column;
  gap: 16px;
  padding: 40px;
  overflow-y: auto;
  max-width: 500px;
}

.launch-title {
  font-size: 28px;
  color: #7aa2f7;
  font-weight: 700;
}

.launch-subtitle {
  font-size: 14px;
  color: #666;
  margin-bottom: 8px;
}

.option-group {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.option-label {
  font-size: 12px;
  color: #888;
  text-transform: uppercase;
  letter-spacing: 1px;
}

.option-buttons {
  display: flex;
  gap: 4px;
}

.opt-btn {
  padding: 6px 14px;
  background: #232436;
  color: #888;
  border: 1px solid #33467c;
  border-radius: 4px;
  cursor: pointer;
  font-size: 13px;
  transition: all 0.15s;
}

.opt-btn:hover {
  background: #2a2b44;
  color: #bbb;
}

.opt-btn.active {
  background: #7aa2f7;
  color: #1a1b2e;
  border-color: #7aa2f7;
  font-weight: 600;
}

.opt-btn.discord-btn.active {
  background: #5865f2;
  border-color: #5865f2;
  color: white;
}

.session-input {
  padding: 6px 10px;
  background: #232436;
  color: #e0e0e0;
  border: 1px solid #33467c;
  border-radius: 4px;
  font-size: 13px;
  outline: none;
}

.session-input:focus {
  border-color: #7aa2f7;
}

/* 資料夾選擇 */
.dir-picker {
  display: flex;
  gap: 4px;
}

.dir-input {
  flex: 1;
}

.dir-browse-btn {
  padding: 6px 10px;
  background: #232436;
  border: 1px solid #33467c;
  border-radius: 4px;
  cursor: pointer;
  font-size: 16px;
  transition: background 0.15s;
}

.dir-browse-btn:hover {
  background: #33467c;
}

/* Session 列表 */
.session-list {
  max-height: 200px;
  overflow-y: auto;
  border: 1px solid #33467c;
  border-radius: 6px;
  background: #1a1b2e;
}

.session-item {
  padding: 8px 12px;
  cursor: pointer;
  border-bottom: 1px solid #232436;
  transition: background 0.15s;
}

.session-item:last-child {
  border-bottom: none;
}

.session-item:hover {
  background: #232436;
}

.session-item.selected {
  background: #33467c;
  border-left: 3px solid #7aa2f7;
}

.session-title {
  font-size: 13px;
  color: #e0e0e0;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.session-time {
  font-size: 11px;
  color: #666;
  margin-top: 2px;
}

.session-loading,
.session-empty {
  padding: 16px;
  text-align: center;
  color: #666;
  font-size: 13px;
}

.launch-btn {
  margin-top: 12px;
  padding: 12px 24px;
  background: #7aa2f7;
  color: #1a1b2e;
  border: none;
  border-radius: 8px;
  font-size: 16px;
  font-weight: 700;
  cursor: pointer;
  transition: background 0.2s;
}

.launch-btn:hover {
  background: #89b4fa;
}

.launch-avatar {
  position: fixed;
  right: 0;
  bottom: 0;
  height: 80%;
  display: flex;
  align-items: flex-end;
  justify-content: flex-end;
}

.avatar-img-large {
  max-width: 100%;
  max-height: 100%;
  image-rendering: pixelated;
}

/* ===== 運行畫面 ===== */
.main-area {
  flex: 1;
  display: flex;
  flex-direction: row;
  overflow: hidden;
}

.terminal-container {
  flex: 1;
  overflow: hidden;
  padding: 8px;
  width: 100%;
  position: relative;
}

.terminal-view {
  height: calc(100% + 100px);
  margin-bottom: -100px;
}

/* Avatar 側邊欄 */
.avatar-sidebar {
  width: 384px;
  min-width: 384px;
  display: flex;
  flex-direction: column;
  align-items: flex-end;
  justify-content: flex-end;
  background: #1a1b2e;
  border-left: 1px solid #33467c;
  overflow: hidden;
}

.busy-text {
  padding: 16px 16px;
  font-size: 13px;
  text-align: left;
  background: #1a1b2e;
  margin-top: -40px;
  position: relative;
  z-index: 5;
}

.busy-text span {
  color: #7aa2f7;
  animation: busy-fade 3s ease-in-out infinite;
}

@keyframes busy-fade {
  0%, 100% { opacity: 0.6; }
  50% { opacity: 1; }
}

.avatar-wrapper {
  height: 100%;
  display: flex;
  align-items: flex-end;
  justify-content: flex-end;
}

.avatar-img {
  height: 100%;
  object-fit: contain;
  object-position: bottom right;
  image-rendering: pixelated;
}

.avatar-status {
  font-size: 12px;
  color: #7aa2f7;
  text-transform: uppercase;
  letter-spacing: 1px;
}

/* 圖片預覽 */
.image-preview-bar {
  display: flex;
  gap: 8px;
  padding: 8px 12px 0 12px;
  background: #232436;
  border-top: 1px solid #33467c;
  overflow-x: auto;
}

.image-preview-item {
  position: relative;
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 4px 8px;
  background: #1a1b2e;
  border: 1px solid #33467c;
  border-radius: 6px;
  flex-shrink: 0;
}

.image-thumb {
  width: 40px;
  height: 40px;
  object-fit: cover;
  border-radius: 4px;
}

.image-name {
  font-size: 11px;
  color: #888;
  max-width: 100px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.image-loading {
  font-size: 11px;
  color: #7aa2f7;
}

.image-remove {
  background: none;
  border: none;
  color: #f7768e;
  font-size: 16px;
  cursor: pointer;
  padding: 0 2px;
  line-height: 1;
}

.image-remove:hover {
  color: #ff99aa;
}

/* 輸入框 */
.input-bar {
  display: flex;
  gap: 8px;
  padding: 8px 12px;
  background: #232436;
  border-top: 1px solid #33467c;
}

.input-textarea {
  flex: 1;
  resize: none;
  padding: 8px 12px;
  background: #1a1b2e;
  color: #e0e0e0;
  border: 1px solid #33467c;
  border-radius: 6px;
  font-family: inherit;
  font-size: 14px;
  line-height: 1.4;
  outline: none;
  max-height: 120px;
  overflow-y: auto;
}

.input-textarea:focus {
  border-color: #7aa2f7;
}

.input-textarea::placeholder {
  color: #555;
}

.send-btn {
  padding: 8px 16px;
  background: #7aa2f7;
  color: #1a1b2e;
  border: none;
  border-radius: 6px;
  cursor: pointer;
  font-weight: bold;
  font-size: 14px;
  align-self: flex-end;
}

.send-btn:hover:not(:disabled) {
  background: #89b4fa;
}

.send-btn:disabled {
  opacity: 0.4;
  cursor: not-allowed;
}

/* 狀態列 */
.status-bar {
  display: flex;
  gap: 8px;
  align-items: center;
  padding: 4px 12px 8px 12px;
  background: #191a2e;
  border-top: 1px solid #232436;
  font-size: 12px;
  color: #666;
}

.status-tag {
  padding: 1px 8px;
  background: #232436;
  border-radius: 3px;
  color: #888;
}

.discord-tag {
  background: #5865f233;
  color: #5865f2;
}

.context-tag {
  background: #9ece6a22;
  color: #9ece6a;
}
</style>
