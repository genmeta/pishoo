import { expect, test } from '@playwright/test'

test.describe.configure({ mode: 'serial' })
test.beforeEach(({}, testInfo) => {
  test.skip(testInfo.project.name !== 'desktop', 'Mutating workflows run once')
})

test('review decisions update the pending queue', async ({ page }) => {
  await page.goto('./approvals')
  const firstRow = page.locator('tbody tr').filter({ has: page.getByRole('button', { name: 'Allow', exact: true }) }).first()
  await expect(firstRow).toBeVisible()
  await firstRow.getByRole('button', { name: 'Allow' }).click()
  await page.getByRole('dialog', { name: /Allow request/ })
    .getByRole('button', { name: 'Allow', exact: true }).click()
  await expect(page.getByText('Request allowed', { exact: true })).toBeVisible()
  const secondRow = page.locator('tbody tr').filter({ has: page.getByRole('button', { name: 'Allow', exact: true }) }).first()
  await expect(secondRow).toBeVisible()
  await secondRow.getByRole('button', { name: 'Deny' }).click()
  await page.getByRole('dialog', { name: /Deny request/ })
    .getByRole('button', { name: 'Deny', exact: true }).click()
  await expect(page.getByText('Request denied', { exact: true })).toBeVisible()
})

test('contact editing and status actions can be restored', async ({ page }) => {
  await page.goto('./contacts')
  await page.getByRole('button', { name: /Alice Chen/ }).click()
  const alias = page.getByLabel('Local alias')
  await alias.fill('Alice QA')
  await page.getByRole('button', { name: 'Save alias' }).click()
  await expect(page.getByText('Alias updated', { exact: true })).toBeVisible()
  await alias.fill('Alice Chen')
  await page.getByRole('button', { name: 'Save alias' }).click()
  await page.getByRole('button', { name: 'Close contact details' }).click()

  await page.getByRole('button', { name: /Build Bot/ }).click()
  await page.getByRole('button', { name: 'Restore' }).click()
  await expect(page.getByText('Contact restored', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Block' }).click()
  await expect(page.getByText('Contact blocked', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Close contact details' }).click()

  await page.locator('.contact-link').filter({ hasText: 'carol.example' }).click()
  await page.getByRole('button', { name: 'Grant chat' }).click()
  await expect(page.getByText('Chat delivery granted', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Close contact details' }).click()

  await page.locator('.contact-link').filter({ hasText: 'legacy-agent.example' }).click()
  await page.getByRole('button', { name: 'Delete', exact: true }).click()
  const deleteDialog = page.getByRole('dialog', { name: 'Delete contacts (1)' })
  await deleteDialog.getByRole('button', { name: 'Delete', exact: true }).click()
  await expect(page.getByText('1 contact deleted', { exact: true })).toBeVisible()
  await expect(page.locator('.contact-link').filter({ hasText: 'legacy-agent.example' })).toHaveCount(0)
})

test('access rules are available as a read-only view', async ({ page }) => {
  await page.goto('./settings/access')
  await expect(page.getByRole('button', { name: 'Add rule' })).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Edit' })).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Delete' })).toHaveCount(0)
  await page.locator('.master-list-items > button').filter({ hasText: '/acl' }).click()
  const ownerRule = page.locator('tbody tr').filter({ hasText: 'owner.local' }).first()
  await expect(ownerRule).toContainText('allow')
})
