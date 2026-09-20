import { onCleanup } from 'solid-js'

export function abortable<T, R>(fetcher: (source: T, signal: AbortSignal) => Promise<R>) {
  let controller: AbortController | undefined
  onCleanup(() => controller?.abort())

  return (source: T) => {
    controller?.abort()
    controller = new AbortController()
    return fetcher(source, controller.signal)
  }
}
