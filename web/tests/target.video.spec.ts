import { expect, test, type APIRequestContext, type Page } from '@playwright/test'

const username = process.env.ONE_KVM_E2E_USERNAME || 'admin'
const password = process.env.ONE_KVM_E2E_PASSWORD || 'admin1234'

type SetupStatus = {
  initialized: boolean
  needs_setup: boolean
}

type DeviceList = {
  video: Array<{
    path: string
    formats: Array<{
      format: string
      resolutions: Array<{
        width: number
        height: number
        fps: number[]
      }>
    }>
  }>
}

type StreamStatus = {
  state: string
  device: string | null
  resolution: [number, number] | null
  clients: number
  target_fps: number
  fps: number
}

type StreamModeStatus = {
  success: boolean
  mode: string
}

type WebRtcStatus = {
  session_count: number
  sessions: Array<{ session_id: string; state: string }>
}

async function readJson<T>(request: APIRequestContext, path: string, options?: Parameters<APIRequestContext['get']>[1]) {
  const response = await request.get(path, options)
  expect(response.ok(), `${path} should succeed`).toBeTruthy()
  return response.json() as Promise<T>
}

async function postJson<T>(request: APIRequestContext, path: string, data?: unknown) {
  const response = await request.post(path, {
    data,
  })
  expect(response.ok(), `${path} should succeed`).toBeTruthy()
  return response.json() as Promise<T>
}

async function ensureInitialized(request: APIRequestContext) {
  const status = await readJson<SetupStatus>(request, '/api/setup')
  if (!status.needs_setup) {
    return
  }

  const devices = await readJson<DeviceList>(request, '/api/devices')
  expect(devices.video.length, 'target should expose at least one video device for setup').toBeGreaterThan(0)

  const device = devices.video[0]
  const format = device.formats[0]
  const resolution = format?.resolutions[0]

  expect(format, 'video device should expose a format').toBeTruthy()
  expect(resolution, 'video format should expose a resolution').toBeTruthy()

  await postJson(request, '/api/setup/init', {
    username,
    password,
    video_device: device.path,
    video_format: format.format,
    video_width: resolution.width,
    video_height: resolution.height,
    video_fps: Math.round(resolution.fps[0] || 30),
    encoder_backend: 'aml',
    hid_backend: 'none',
    msd_enabled: false,
  })
}

async function login(page: Page) {
  await page.goto('/login')
  await page.locator('#username').fill(username)
  await page.locator('#password').fill(password)
  await page.getByRole('button', { name: /login/i }).click()
  await expect(page).toHaveURL(/\/$/)
  await expect(page.getByText('One-KVM')).toBeVisible()
}

async function fetchStreamStatus(page: Page): Promise<StreamStatus> {
  return page.evaluate(async () => {
    const response = await fetch('/api/stream/status', { credentials: 'include' })
    if (!response.ok) {
      throw new Error(`stream status failed: ${response.status}`)
    }
    return response.json()
  })
}

async function fetchStreamMode(page: Page): Promise<StreamModeStatus> {
  return page.evaluate(async () => {
    const response = await fetch('/api/stream/mode', { credentials: 'include' })
    if (!response.ok) {
      throw new Error(`stream mode failed: ${response.status}`)
    }
    return response.json()
  })
}

async function fetchWebRtcStatus(page: Page): Promise<WebRtcStatus> {
  return page.evaluate(async () => {
    const response = await fetch('/api/webrtc/status', { credentials: 'include' })
    if (!response.ok) {
      throw new Error(`webrtc status failed: ${response.status}`)
    }
    return response.json()
  })
}

test.describe.serial('target video verification', () => {
  test.beforeAll(async ({ request }) => {
    await ensureInitialized(request)
  })

  test('logs in and renders an active H264 WebRTC video session', async ({ page }) => {
    await login(page)

    await page.evaluate(async () => {
      const response = await fetch('/api/stream/mode', {
        method: 'POST',
        credentials: 'include',
        headers: {
          'Content-Type': 'application/json',
        },
        body: JSON.stringify({ mode: 'h264' }),
      })
      if (!response.ok) {
        throw new Error(`failed to switch to h264: ${response.status}`)
      }
    })

    await expect(page.getByTestId('video-status-card').nth(1)).toBeVisible()
    await expect(page.getByTestId('webrtc-stream')).toBeVisible()

    await expect
      .poll(async () => {
        const mode = await fetchStreamMode(page)
        return mode.mode
      })
      .toBe('h264')

    await expect
      .poll(async () => {
        const status = await fetchStreamStatus(page)
        return {
          state: status.state,
          hasDevice: Boolean(status.device),
          hasResolution: status.resolution !== null,
        }
      }, {
        message: 'stream status should expose a capture device and resolution for WebRTC',
      })
      .toMatchObject({
        state: 'ready',
        hasDevice: true,
        hasResolution: true,
      })

    const status = await fetchStreamStatus(page)
    expect(status.device).toBeTruthy()
    expect(status.resolution).not.toBeNull()

    await expect
      .poll(async () => {
        const current = await fetchWebRtcStatus(page)
        return current.session_count
      }, {
        message: 'browser should establish a WebRTC session for H264 video',
      })
      .toBeGreaterThan(0)

    await expect
      .poll(async () => page.evaluate(() => {
        const video = document.querySelector('[data-testid="webrtc-stream"]') as HTMLVideoElement | null
        return {
          readyState: video?.readyState ?? 0,
          videoWidth: video?.videoWidth ?? 0,
          videoHeight: video?.videoHeight ?? 0,
        }
      }), {
        message: 'webrtc video element should decode H264 frames',
      })
      .toMatchObject({
        readyState: 4,
        videoWidth: expect.any(Number),
        videoHeight: expect.any(Number),
      })

    const decoded = await page.evaluate(() => {
      const video = document.querySelector('[data-testid="webrtc-stream"]') as HTMLVideoElement | null
      return {
        videoWidth: video?.videoWidth ?? 0,
        videoHeight: video?.videoHeight ?? 0,
      }
    })
    expect(decoded.videoWidth).toBeGreaterThan(0)
    expect(decoded.videoHeight).toBeGreaterThan(0)
  })
})
