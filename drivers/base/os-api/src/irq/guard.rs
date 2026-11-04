// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ownership of one IRQ registration and its exact provider release identity.

use alloc::sync::Arc;
use core::fmt;

use super::{IrqHandler, IrqHandlerToken, IrqOp, IrqResource, IrqThreadHandler, Virq};
use crate::{ReleaseHook, ResResult};

enum IrqProvider {
    Static(&'static dyn IrqOp),
    Shared(Arc<dyn IrqOp>),
}

impl IrqProvider {
    fn get(&self) -> &dyn IrqOp {
        match self {
            Self::Static(provider) => *provider,
            Self::Shared(provider) => provider.as_ref(),
        }
    }
}

enum IrqOwner {
    Provider {
        provider: IrqProvider,
        resource: IrqResource,
        token: IrqHandlerToken,
    },
    Hook {
        virq: Virq,
        release: Option<ReleaseHook>,
    },
}

/// One registered device interrupt action, synchronously released on Drop.
///
/// Stop device access first and drop in sleepable task context without locks
/// needed by callbacks. Owning an action does not imply ownership of the line.
/// Provider-backed guards retain the exact resource/token until release.
pub struct DevIrq {
    owner: IrqOwner,
}

impl DevIrq {
    /// Construct a guard with an externally supplied matching release hook.
    ///
    /// The caller must pair the hook with this registration and ensure its
    /// normal return establishes quiescence. None represents no release work.
    pub fn new(virq: Virq, release: Option<ReleaseHook>) -> Self {
        Self {
            owner: IrqOwner::Hook { virq, release },
        }
    }

    /// Register through a static provider without allocating a release hook.
    pub fn request_with(
        provider: &'static dyn IrqOp,
        resource: IrqResource,
        handler: Arc<dyn IrqHandler>,
    ) -> ResResult<Self> {
        let token = provider.request_irq(resource, handler)?;
        Ok(Self {
            owner: IrqOwner::Provider {
                provider: IrqProvider::Static(provider),
                resource,
                token,
            },
        })
    }

    /// Register while retaining the provider Arc through synchronous release.
    ///
    /// Shared refers to provider ownership, not an IRQ sharing policy.
    pub fn request_with_shared_provider(
        provider: Arc<dyn IrqOp>,
        resource: IrqResource,
        handler: Arc<dyn IrqHandler>,
    ) -> ResResult<Self> {
        let token = provider.request_irq(resource, handler)?;
        Ok(Self {
            owner: IrqOwner::Provider {
                provider: IrqProvider::Shared(provider),
                resource,
                token,
            },
        })
    }

    /// Register a primary and threaded handler through a static provider.
    ///
    /// Providers without threaded support return Unsupported without a guard.
    pub fn request_threaded_with(
        provider: &'static dyn IrqOp,
        resource: IrqResource,
        primary: Arc<dyn IrqHandler>,
        thread: Arc<dyn IrqThreadHandler>,
        name: Option<&'static str>,
    ) -> ResResult<Self> {
        let token = provider.request_threaded_irq(resource, primary, thread, name)?;
        Ok(Self {
            owner: IrqOwner::Provider {
                provider: IrqProvider::Static(provider),
                resource,
                token,
            },
        })
    }

    /// Register a threaded handler with the provider's default primary.
    pub fn request_threaded_default_with(
        provider: &'static dyn IrqOp,
        resource: IrqResource,
        thread: Arc<dyn IrqThreadHandler>,
        name: Option<&'static str>,
    ) -> ResResult<Self> {
        let token = provider.request_threaded_irq_default(resource, thread, name)?;
        Ok(Self {
            owner: IrqOwner::Provider {
                provider: IrqProvider::Static(provider),
                resource,
                token,
            },
        })
    }

    /// The OS-visible IRQ for this registration.
    pub fn virq(&self) -> Virq {
        match &self.owner {
            IrqOwner::Provider { resource, .. } => resource.virq(),
            IrqOwner::Hook { virq, .. } => *virq,
        }
    }
}

impl Drop for DevIrq {
    fn drop(&mut self) {
        match &mut self.owner {
            IrqOwner::Provider {
                provider,
                resource,
                token,
            } => {
                provider.get().release_irq(*resource, *token);
            }
            IrqOwner::Hook { release, .. } => {
                if let Some(release) = release.take() {
                    release();
                }
            }
        }
    }
}

impl fmt::Debug for DevIrq {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DevIrq")
            .field("virq", &self.virq())
            .finish_non_exhaustive()
    }
}
