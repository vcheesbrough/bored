use leptos::prelude::*;

/// A small confirmation dialog that blocks a destructive action.
///
/// Set `show` to `true` to open it. `on_confirm` fires only if the user
/// clicks "Delete"; clicking "Cancel" or the backdrop dismisses without action.
#[component]
pub fn ConfirmModal(show: RwSignal<bool>, on_confirm: Callback<()>) -> impl IntoView {
    let cancel = move || show.set(false);

    view! {
        <Show when=move || show.get() fallback=|| ()>
            <div class="confirm-backdrop" on:click=move |_| cancel()>
                <div class="confirm-dialog" on:click=|ev| ev.stop_propagation()>
                    <p class="confirm-title">"Delete this card?"</p>
                    <p class="confirm-body">"This cannot be undone."</p>
                    <div class="confirm-actions">
                        <button
                            class="btn-ghost"
                            on:click=move |_| cancel()
                        >"Cancel"</button>
                        <button
                            class="btn-danger"
                            on:click=move |_| {
                                show.set(false);
                                on_confirm.run(());
                            }
                        >"Delete"</button>
                    </div>
                </div>
            </div>
        </Show>
    }
}
