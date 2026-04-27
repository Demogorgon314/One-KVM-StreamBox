// WebSocket composable for real-time event streaming
//
// Usage:
//   const { connected, on, off } = useWebSocket()
//   on('stream.state_changed', (data) => { ... })

import { ref } from 'vue'
import { buildWsUrl, WS_RECONNECT_DELAY } from '@/types/websocket'

export interface WsEvent {
  event: string
  data: any
}

type EventHandler = (data: any) => void

let wsInstance: WebSocket | null = null
let handlers = new Map<string, EventHandler[]>()
let subscribedTopics: string[] = []
let reconnectTimer: ReturnType<typeof setTimeout> | null = null
let connectPromise: Promise<void> | null = null
const connected = ref(false)
const reconnectAttempts = ref(0)
const networkError = ref(false)
const networkErrorMessage = ref<string | null>(null)

function getSubscribedTopics(): string[] {
  return Array.from(handlers.entries())
    .filter(([, eventHandlers]) => eventHandlers.length > 0)
    .map(([event]) => event)
    .sort()
}

function arraysEqual(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((value, index) => value === b[index])
}

function syncSubscriptions() {
  const topics = getSubscribedTopics()

  if (arraysEqual(topics, subscribedTopics)) {
    return
  }

  subscribedTopics = topics

  if (wsInstance && wsInstance.readyState === WebSocket.OPEN) {
    subscribe(topics)
  }
}

function scheduleReconnect() {
  if (reconnectTimer !== null) {
    clearTimeout(reconnectTimer)
  }
  reconnectAttempts.value++
  reconnectTimer = setTimeout(() => {
    reconnectTimer = null
    void connect()
  }, WS_RECONNECT_DELAY)
}

function connect(): Promise<void> {
  if (wsInstance && wsInstance.readyState === WebSocket.OPEN) {
    syncSubscriptions()
    return Promise.resolve()
  }

  if (wsInstance && wsInstance.readyState === WebSocket.CONNECTING && connectPromise) {
    return connectPromise
  }

  const url = buildWsUrl('/api/ws')

  try {
    const ws = new WebSocket(url)
    wsInstance = ws

    connectPromise = new Promise((resolve) => {
      ws.onopen = () => {
        if (wsInstance !== ws) {
          resolve()
          return
        }

      connected.value = true
      networkError.value = false
      networkErrorMessage.value = null
      reconnectAttempts.value = 0
        connectPromise = null

      syncSubscriptions()
        resolve()
    }

      ws.onmessage = (e) => {
        if (wsInstance !== ws) return
      try {
        const event: WsEvent = JSON.parse(e.data)

        if (event.event === 'error') {
          console.error('[WebSocket] Server error:', event.data?.message)
        } else {
          handleEvent(event)
        }
      } catch (err) {
        console.error('[WebSocket] Failed to parse message:', err)
      }
    }

      ws.onclose = () => {
        if (wsInstance !== ws) return

      connected.value = false
      networkError.value = true
        wsInstance = null
        connectPromise = null

      // Auto-reconnect with infinite retry
        scheduleReconnect()
    }

      ws.onerror = () => {
        if (wsInstance !== ws) return
      networkError.value = true
      networkErrorMessage.value = 'Network connection failed'
    }
    })

    return connectPromise
  } catch (err) {
    console.error('[WebSocket] Failed to create connection:', err)
    return Promise.resolve()
  }
}

function disconnect() {
  if (reconnectTimer !== null) {
    clearTimeout(reconnectTimer)
    reconnectTimer = null
  }

  const ws = wsInstance
  wsInstance = null
  connectPromise = null
  subscribedTopics = []
  connected.value = false

  if (ws) {
    ws.close()
  }
}

async function reconnect() {
  disconnect()
  await new Promise(resolve => setTimeout(resolve, 100))
  await connect()
}

function subscribe(topics: string[]) {
  if (wsInstance && wsInstance.readyState === WebSocket.OPEN) {
    wsInstance.send(JSON.stringify({
      type: 'subscribe',
      payload: { topics }
    }))
  }
}

function on(event: string, handler: EventHandler) {
  if (!handlers.has(event)) {
    handlers.set(event, [])
  }
  handlers.get(event)!.push(handler)
  syncSubscriptions()
}

function off(event: string, handler: EventHandler) {
  const eventHandlers = handlers.get(event)
  if (eventHandlers) {
    const index = eventHandlers.indexOf(handler)
    if (index > -1) {
      eventHandlers.splice(index, 1)
    }
    if (eventHandlers.length === 0) {
      handlers.delete(event)
    }
  }
  syncSubscriptions()
}

function handleEvent(payload: WsEvent) {
  const eventName = payload.event
  const eventHandlers = handlers.get(eventName)

  if (eventHandlers) {
    eventHandlers.forEach(handler => {
      try {
        handler(payload.data)
      } catch (err) {
        console.error(`[WebSocket] Error in handler for ${eventName}:`, err)
      }
    })
  }
  // Silently ignore events without handlers
}

export function useWebSocket() {
  // Connection is now triggered manually by components after registering handlers

  return {
    connected,
    reconnectAttempts,
    networkError,
    networkErrorMessage,
    on,
    off,
    subscribe,
    connect,
    disconnect,
    reconnect,
  }
}

// Global lifecycle - disconnect when page unloads
if (typeof window !== 'undefined') {
  window.addEventListener('beforeunload', () => {
    disconnect()
  })
}
