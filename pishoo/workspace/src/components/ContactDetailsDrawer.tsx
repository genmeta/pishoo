import { Show, createResource } from 'solid-js'

import { api } from '../api/client'
import { chatApi } from '../chat/api'
import { contactClassLabel, useI18n } from '../i18n'
import { abortable } from '../lib/abortable'
import { chatCapabilityLabel, chatDirectionLabel, chatDirectionState } from '../lib/chatCapability'
import { contactDisplayName, contactStatus, displayIdentityName, errorMessage } from '../lib/format'
import { Avatar, ErrorState, LoadingState, StatusBadge } from './Ui'

export default function ContactDetailsDrawer(props: {
  contactName: string | null
  onClose: () => void
}) {
  const { date, t } = useI18n()
  const loadContact = abortable((name: string, signal: AbortSignal) => api.contact(name, signal))
  const loadProfile = abortable((name: string, signal: AbortSignal) => api.publicProfile(name, signal))
  const loadCapability = abortable((name: string, signal: AbortSignal) => chatApi.capability(name, signal))
  const [contact] = createResource(() => props.contactName, loadContact)
  const [profile] = createResource(() => contact()?.name ?? null, loadProfile)
  const [capability] = createResource(() => contact()?.name ?? null, loadCapability)
  const resolvedProfile = () => profile.error ? undefined : profile()
  const identityName = () => displayIdentityName(contact()?.name ?? props.contactName ?? '')
  const displayName = () => contactDisplayName(
    contact()?.name ?? props.contactName ?? '', contact()?.alias, resolvedProfile()?.display_name,
  )

  return (
    <Show when={props.contactName}>
      <div class="drawer-backdrop" onMouseDown={(event) => event.target === event.currentTarget && props.onClose()}>
        <aside class="detail-drawer" aria-label={t('contacts.details')}>
          <header class="drawer-header">
            <h2>{displayName()}</h2>
            <button class="icon-button" type="button" aria-label={t('contacts.closeDetails')} onClick={props.onClose}>
              ×
            </button>
          </header>

          <Show when={contact.loading}>
            <LoadingState label={t('contacts.loadingOne')} />
          </Show>
          <Show when={contact.error}>
            <ErrorState message={errorMessage(contact.error, t('common.unexpectedError'))} />
          </Show>
          <Show when={contact()} keyed>
            {(item) => (
              <div class="drawer-content">
                <div class="contact-heading">
                  <Avatar name={displayName()} src={resolvedProfile()?.avatar_url} large />
                  <span class="contact-identity-detail">
                    <strong>{displayName()}</strong>
                    <Show when={identityName() !== displayName()}><small>{identityName()}</small></Show>
                  </span>
                  <StatusBadge value={contactStatus(item.status)} />
                </div>

                <dl class="metadata-grid">
                  <div><dt>{t('contacts.alias')}</dt><dd>{item.alias || t('common.none')}</dd></div>
                  <div><dt>{t('contacts.class')}</dt><dd>{contactClassLabel(t, item.class)}</dd></div>
                  <div><dt>{t('contacts.updated')}</dt><dd>{date(item.updated_at)}</dd></div>
                  <div><dt>{t('contacts.created')}</dt><dd>{date(item.created_at)}</dd></div>
                  <div><dt>{t('contacts.expires')}</dt><dd>{date(item.expired_after)}</dd></div>
                  <div class="wide"><dt>{t('contacts.subjectId')}</dt><dd><code>{item.subject_id}</code></dd></div>
                </dl>

                <section class="detail-section capability-status-section">
                  <h3>{t('contacts.capabilities')}</h3>
                  <div class="contact-capability-card">
                    <strong>{t('contacts.chatCapability')}</strong>
                    <span class="status">{chatCapabilityLabel(item, t, capability())}</span>
                  </div>
                  <div class="capability-direction-grid">
                    <div class="capability-direction-row">
                      <strong>{t('contacts.chatDirectionIncoming')}</strong>
                      <span class="status">{chatDirectionLabel(chatDirectionState('incoming', item, capability()), t)}</span>
                    </div>
                    <div class="capability-direction-row">
                      <strong>{t('contacts.chatDirectionOutgoing')}</strong>
                      <span class="status">{chatDirectionLabel(chatDirectionState('outgoing', item, capability()), t)}</span>
                    </div>
                  </div>
                </section>
              </div>
            )}
          </Show>
        </aside>
      </div>
    </Show>
  )
}
