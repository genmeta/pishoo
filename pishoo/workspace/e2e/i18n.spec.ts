import { expect, test } from '@playwright/test'

test.beforeEach(({}, testInfo) => {
  test.skip(testInfo.project.name !== 'desktop', 'Locale persistence only needs one browser profile')
})

test('switches between English and Chinese and remembers the choice', async ({ page }) => {
  await page.goto('./access/reviews')
  await expect(page.getByRole('heading', { name: 'Reviews' })).toBeVisible()

  await page.getByRole('button', { name: 'Chinese' }).click()
  await expect(page.getByRole('heading', { name: '审批' })).toBeVisible()
  await expect(page.getByRole('button', { name: '联系人' })).toBeVisible()
  await expect(page.locator('html')).toHaveAttribute('lang', 'zh-CN')

  await page.getByRole('button', { name: '联系人' }).click()
  await page.getByRole('button', { name: /Alice Chen/ }).click()
  await expect(page.getByText('本地别名')).toBeVisible()
  await page.getByRole('button', { name: '关闭联系人详情' }).click()

  await page.getByRole('button', { name: '访问规则' }).click()
  await page.getByRole('button', { name: '添加规则' }).first().click()
  await expect(page.getByRole('dialog', { name: '访问规则' })).toBeVisible()
  await page.getByRole('button', { name: '取消' }).click()

  await page.reload()
  await expect(page.getByRole('heading', { name: '访问规则' })).toBeVisible()
  await expect(page.getByRole('button', { name: '中文' })).toHaveAttribute('aria-pressed', 'true')

  await page.getByRole('button', { name: '英文' }).click()
  await expect(page.getByRole('heading', { name: 'Access rules' })).toBeVisible()
  await expect(page.locator('html')).toHaveAttribute('lang', 'en')
})

test.describe('browser language default', () => {
  test.use({ locale: 'zh-CN' })

  test('uses Chinese for a first-time Chinese browser', async ({ page }) => {
    await page.goto('./access/reviews')
    await expect(page.getByRole('heading', { name: '审批' })).toBeVisible()
    await expect(page.locator('html')).toHaveAttribute('lang', 'zh-CN')
  })
})
