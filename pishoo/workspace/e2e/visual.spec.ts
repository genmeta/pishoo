import { expect, test } from '@playwright/test'
import type { Page } from '@playwright/test'

async function expectNoPageOverflow(page: Page): Promise<void> {
  const dimensions = await page.evaluate(() => ({
    viewport: document.documentElement.clientWidth,
    content: document.documentElement.scrollWidth,
    offenders: [...document.querySelectorAll<HTMLElement>('body *')]
      .map((element) => {
        const rect = element.getBoundingClientRect()
        return {
          element: `${element.tagName.toLowerCase()}.${element.className}`,
          left: Math.round(rect.left),
          right: Math.round(rect.right),
          width: Math.round(rect.width),
        }
      })
      .filter(({ left, right }) => left < 0 || right > document.documentElement.clientWidth)
      .slice(0, 12),
  }))
  expect(dimensions.content, JSON.stringify(dimensions.offenders, null, 2)).toBeLessThanOrEqual(
    dimensions.viewport,
  )
}

async function expectNoVerticalPageOverflow(page: Page): Promise<void> {
  const dimensions = await page.evaluate(() => ({
    viewport: document.documentElement.clientHeight,
    content: document.documentElement.scrollHeight,
  }))
  expect(dimensions.content).toBeLessThanOrEqual(dimensions.viewport)
}

test('core pages remain usable at the target viewport', async ({ page }, testInfo) => {
  await page.goto('./')
  await expect(page.getByRole('heading', { name: 'Reviews' })).toBeVisible()
  await expect(page.locator('.toolbar-meta')).not.toHaveText('Updating')
  await expect(page.getByRole('region', { name: 'pending reviews' })).toBeVisible()
  await expectNoPageOverflow(page)
  await page.screenshot({ path: testInfo.outputPath('reviews.png'), fullPage: true })

  await page.getByRole('button', { name: 'Contacts' }).click()
  await expect(page.getByRole('heading', { name: 'Contacts' })).toBeVisible()
  await expect(page.getByRole('button', { name: /Alice Chen/ })).toBeVisible()
  await expectNoPageOverflow(page)
  await page.screenshot({ path: testInfo.outputPath('contacts.png'), fullPage: true })

  await page.getByRole('button', { name: 'Access rules' }).click()
  await expect(page.getByRole('heading', { name: 'Access rules' })).toBeVisible()
  await expect(page.locator('.master-list-items > button').first()).toBeVisible()
  await expectNoPageOverflow(page)
  if (testInfo.project.name === 'desktop') await expectNoVerticalPageOverflow(page)

  await page.getByRole('button', { name: 'Add rule' }).first().click()
  await expect(page.getByRole('dialog', { name: 'Access rule' })).toBeVisible()
  await expectNoPageOverflow(page)
  await page.screenshot({ path: testInfo.outputPath('rule-dialog.png'), fullPage: true })
})
