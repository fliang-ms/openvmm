// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common processor support for hardware-isolated partitions.

pub mod apic;
pub mod tlb_lock;

use super::UhEmulationState;
use super::UhProcessor;
use super::UhRunVpError;
use crate::CvmVtl1State;
use crate::GuestVsmState;
use crate::GuestVtl;
use crate::InitialVpContextOperation;
use crate::TlbFlushLockAccess;
use crate::VpStartEnableVtl;
use crate::WakeReason;
use crate::processor::HardwareIsolatedBacking;
use crate::processor::UhHypercallHandler;
use crate::validate_vtl_gpa_flags;
use guestmem::GuestMemory;
use hv1_emulator::RequestInterrupt;
use hv1_hypercall::HvRepResult;
use hv1_structs::ProcessorSet;
use hvdef::HvCacheType;
use hvdef::HvError;
use hvdef::HvMapGpaFlags;
use hvdef::HvRegisterVsmPartitionConfig;
use hvdef::HvRegisterVsmVpSecureVtlConfig;
use hvdef::HvResult;
use hvdef::HvSynicSint;
use hvdef::HvVtlEntryReason;
use hvdef::HvX64RegisterName;
use hvdef::Vtl;
use hvdef::hypercall::HostVisibilityType;
use hvdef::hypercall::HvFlushFlags;
use hvdef::hypercall::TranslateGvaResultCode;
use std::iter::zip;
use virt::Processor;
use virt::io::CpuIo;
use virt::vp::AccessVpState;
use virt::x86::MsrError;
use virt_support_x86emu::emulate::TranslateGvaSupport;
use virt_support_x86emu::translate::TranslateCachingInfo;
use virt_support_x86emu::translate::TranslationRegisters;
use zerocopy::FromZeros;

impl<T, B: HardwareIsolatedBacking> UhHypercallHandler<'_, '_, T, B> {
    fn validate_register_access(
        &mut self,
        vtl: GuestVtl,
        name: hvdef::HvRegisterName,
    ) -> HvResult<()> {
        match name.into() {
            HvX64RegisterName::Star
            | HvX64RegisterName::Lstar
            | HvX64RegisterName::Cstar
            | HvX64RegisterName::SysenterCs
            | HvX64RegisterName::SysenterEip
            | HvX64RegisterName::SysenterEsp
            | HvX64RegisterName::Sfmask
            | HvX64RegisterName::Xfem
            | HvX64RegisterName::KernelGsBase
            | HvX64RegisterName::Efer
            | HvX64RegisterName::Cr0
            | HvX64RegisterName::Cr2
            | HvX64RegisterName::Cr3
            | HvX64RegisterName::Cr4
            | HvX64RegisterName::Cr8
            | HvX64RegisterName::Dr0
            | HvX64RegisterName::Dr1
            | HvX64RegisterName::Dr2
            | HvX64RegisterName::Dr3
            | HvX64RegisterName::Dr7
            | HvX64RegisterName::Es
            | HvX64RegisterName::Cs
            | HvX64RegisterName::Ss
            | HvX64RegisterName::Ds
            | HvX64RegisterName::Fs
            | HvX64RegisterName::Gs
            | HvX64RegisterName::Tr
            | HvX64RegisterName::Ldtr
            | HvX64RegisterName::Gdtr
            | HvX64RegisterName::Idtr
            | HvX64RegisterName::Rip
            | HvX64RegisterName::Rflags
            | HvX64RegisterName::Rax
            | HvX64RegisterName::Rcx
            | HvX64RegisterName::Rdx
            | HvX64RegisterName::Rbx
            | HvX64RegisterName::Rsp
            | HvX64RegisterName::Rbp
            | HvX64RegisterName::Rsi
            | HvX64RegisterName::Rdi
            | HvX64RegisterName::R8
            | HvX64RegisterName::R9
            | HvX64RegisterName::R10
            | HvX64RegisterName::R11
            | HvX64RegisterName::R12
            | HvX64RegisterName::R13
            | HvX64RegisterName::R14
            | HvX64RegisterName::R15
            | HvX64RegisterName::Pat => {
                // Architectural registers can only be accessed by a higher VTL.
                if vtl >= self.intercepted_vtl {
                    return Err(HvError::AccessDenied);
                }
                Ok(())
            }
            HvX64RegisterName::TscAux => {
                // Architectural registers can only be accessed by a higher VTL.
                if vtl >= self.intercepted_vtl {
                    return Err(HvError::AccessDenied);
                }

                if self.vp.partition.caps.tsc_aux {
                    Ok(())
                } else {
                    Err(HvError::InvalidParameter)
                }
            }
            _ => Ok(()),
        }
    }

    fn reg_access_error_to_hv_err(err: crate::processor::vp_state::Error) -> HvError {
        tracing::trace!(?err, "failed on register access");

        match err {
            super::vp_state::Error::SetRegisters(_) => HvError::OperationFailed,
            super::vp_state::Error::GetRegisters(_) => HvError::OperationFailed,
            super::vp_state::Error::SetEfer(_, _) => HvError::InvalidRegisterValue,
            super::vp_state::Error::Unimplemented(_) => HvError::InvalidParameter,
            super::vp_state::Error::InvalidApicBase(_) => HvError::InvalidRegisterValue,
        }
    }

    fn get_vp_register(
        &mut self,
        vtl: GuestVtl,
        name: hvdef::HvRegisterName,
    ) -> HvResult<hvdef::HvRegisterValue> {
        self.validate_register_access(vtl, name)?;
        // TODO: when get vp register i.e. in access vp state gets refactored,
        // clean this up.

        match name.into() {
            HvX64RegisterName::VsmCodePageOffsets => Ok(u64::from(
                self.vp.backing.cvm_state_mut().hv[vtl].vsm_code_page_offsets(true),
            )
            .into()),
            HvX64RegisterName::VsmCapabilities => Ok(u64::from(
                hvdef::HvRegisterVsmCapabilities::new()
                    .with_deny_lower_vtl_startup(true)
                    .with_dr6_shared(self.vp.partition.hcl.dr6_shared()),
            )
            .into()),
            HvX64RegisterName::VsmVpSecureConfigVtl0 => {
                Ok(u64::from(self.vp.get_vsm_vp_secure_config_vtl(vtl, GuestVtl::Vtl0)?).into())
            }
            HvX64RegisterName::VpAssistPage => Ok(self.vp.backing.cvm_state_mut().hv[vtl]
                .vp_assist_page()
                .into()),
            virt_msr @ (HvX64RegisterName::Star
            | HvX64RegisterName::Lstar
            | HvX64RegisterName::Cstar
            | HvX64RegisterName::SysenterCs
            | HvX64RegisterName::SysenterEip
            | HvX64RegisterName::SysenterEsp
            | HvX64RegisterName::Sfmask
            | HvX64RegisterName::KernelGsBase) => {
                let msrs = self
                    .vp
                    .access_state(vtl.into())
                    .virtual_msrs()
                    .map_err(Self::reg_access_error_to_hv_err)?;
                match virt_msr {
                    HvX64RegisterName::Star => Ok(msrs.star.into()),
                    HvX64RegisterName::Lstar => Ok(msrs.lstar.into()),
                    HvX64RegisterName::Cstar => Ok(msrs.cstar.into()),
                    HvX64RegisterName::SysenterCs => Ok(msrs.sysenter_cs.into()),
                    HvX64RegisterName::SysenterEip => Ok(msrs.sysenter_eip.into()),
                    HvX64RegisterName::SysenterEsp => Ok(msrs.sysenter_esp.into()),
                    HvX64RegisterName::Sfmask => Ok(msrs.sfmask.into()),
                    HvX64RegisterName::KernelGsBase => Ok(msrs.kernel_gs_base.into()),
                    _ => unreachable!(),
                }
            }
            HvX64RegisterName::Xfem => Ok(self
                .vp
                .access_state(vtl.into())
                .xcr()
                .map_err(Self::reg_access_error_to_hv_err)?
                .value
                .into()),
            HvX64RegisterName::TscAux => Ok(self
                .vp
                .access_state(vtl.into())
                .tsc_aux()
                .map_err(Self::reg_access_error_to_hv_err)?
                .value
                .into()),
            register @ (HvX64RegisterName::Efer
            | HvX64RegisterName::Cr0
            | HvX64RegisterName::Cr2
            | HvX64RegisterName::Cr3
            | HvX64RegisterName::Cr4
            | HvX64RegisterName::Cr8
            | HvX64RegisterName::Es
            | HvX64RegisterName::Cs
            | HvX64RegisterName::Ss
            | HvX64RegisterName::Ds
            | HvX64RegisterName::Fs
            | HvX64RegisterName::Gs
            | HvX64RegisterName::Tr
            | HvX64RegisterName::Ldtr
            | HvX64RegisterName::Gdtr
            | HvX64RegisterName::Idtr
            | HvX64RegisterName::Rip
            | HvX64RegisterName::Rflags
            | HvX64RegisterName::Rax
            | HvX64RegisterName::Rcx
            | HvX64RegisterName::Rdx
            | HvX64RegisterName::Rbx
            | HvX64RegisterName::Rsp
            | HvX64RegisterName::Rbp
            | HvX64RegisterName::Rsi
            | HvX64RegisterName::Rdi
            | HvX64RegisterName::R8
            | HvX64RegisterName::R9
            | HvX64RegisterName::R10
            | HvX64RegisterName::R11
            | HvX64RegisterName::R12
            | HvX64RegisterName::R13
            | HvX64RegisterName::R14
            | HvX64RegisterName::R15) => {
                let registers = self
                    .vp
                    .access_state(vtl.into())
                    .registers()
                    .map_err(Self::reg_access_error_to_hv_err)?;
                match register {
                    HvX64RegisterName::Efer => Ok(registers.efer.into()),
                    HvX64RegisterName::Cr0 => Ok(registers.cr0.into()),
                    HvX64RegisterName::Cr2 => Ok(registers.cr2.into()),
                    HvX64RegisterName::Cr3 => Ok(registers.cr3.into()),
                    HvX64RegisterName::Cr4 => Ok(registers.cr4.into()),
                    HvX64RegisterName::Cr8 => Ok(registers.cr8.into()),
                    HvX64RegisterName::Es => {
                        Ok(hvdef::HvX64SegmentRegister::from(registers.es).into())
                    }
                    HvX64RegisterName::Cs => {
                        Ok(hvdef::HvX64SegmentRegister::from(registers.cs).into())
                    }
                    HvX64RegisterName::Ss => {
                        Ok(hvdef::HvX64SegmentRegister::from(registers.ss).into())
                    }
                    HvX64RegisterName::Ds => {
                        Ok(hvdef::HvX64SegmentRegister::from(registers.ds).into())
                    }
                    HvX64RegisterName::Fs => {
                        Ok(hvdef::HvX64SegmentRegister::from(registers.fs).into())
                    }
                    HvX64RegisterName::Gs => {
                        Ok(hvdef::HvX64SegmentRegister::from(registers.gs).into())
                    }
                    HvX64RegisterName::Tr => {
                        Ok(hvdef::HvX64SegmentRegister::from(registers.tr).into())
                    }
                    HvX64RegisterName::Ldtr => {
                        Ok(hvdef::HvX64SegmentRegister::from(registers.ldtr).into())
                    }
                    HvX64RegisterName::Gdtr => {
                        Ok(hvdef::HvX64TableRegister::from(registers.gdtr).into())
                    }
                    HvX64RegisterName::Idtr => {
                        Ok(hvdef::HvX64TableRegister::from(registers.idtr).into())
                    }
                    HvX64RegisterName::Rip => Ok(registers.rip.into()),
                    HvX64RegisterName::Rflags => Ok(registers.rflags.into()),
                    HvX64RegisterName::Rax => Ok(registers.rax.into()),
                    HvX64RegisterName::Rcx => Ok(registers.rcx.into()),
                    HvX64RegisterName::Rdx => Ok(registers.rdx.into()),
                    HvX64RegisterName::Rbx => Ok(registers.rbx.into()),
                    HvX64RegisterName::Rsp => Ok(registers.rsp.into()),
                    HvX64RegisterName::Rbp => Ok(registers.rbp.into()),
                    HvX64RegisterName::Rsi => Ok(registers.rsi.into()),
                    HvX64RegisterName::Rdi => Ok(registers.rdi.into()),
                    HvX64RegisterName::R8 => Ok(registers.r8.into()),
                    HvX64RegisterName::R9 => Ok(registers.r9.into()),
                    HvX64RegisterName::R10 => Ok(registers.r10.into()),
                    HvX64RegisterName::R11 => Ok(registers.r11.into()),
                    HvX64RegisterName::R12 => Ok(registers.r12.into()),
                    HvX64RegisterName::R13 => Ok(registers.r13.into()),
                    HvX64RegisterName::R14 => Ok(registers.r14.into()),
                    HvX64RegisterName::R15 => Ok(registers.r15.into()),
                    _ => unreachable!(),
                }
            }
            debug_reg @ (HvX64RegisterName::Dr0
            | HvX64RegisterName::Dr1
            | HvX64RegisterName::Dr2
            | HvX64RegisterName::Dr3
            | HvX64RegisterName::Dr7) => {
                let debug_regs = self
                    .vp
                    .access_state(vtl.into())
                    .debug_regs()
                    .map_err(Self::reg_access_error_to_hv_err)?;
                match debug_reg {
                    HvX64RegisterName::Dr0 => Ok(debug_regs.dr0.into()),
                    HvX64RegisterName::Dr1 => Ok(debug_regs.dr1.into()),
                    HvX64RegisterName::Dr2 => Ok(debug_regs.dr2.into()),
                    HvX64RegisterName::Dr3 => Ok(debug_regs.dr3.into()),
                    HvX64RegisterName::Dr7 => Ok(debug_regs.dr7.into()),
                    _ => unreachable!(),
                }
            }
            HvX64RegisterName::Pat => Ok(self
                .vp
                .access_state(vtl.into())
                .pat()
                .map_err(Self::reg_access_error_to_hv_err)?
                .value
                .into()),
            synic_reg @ (HvX64RegisterName::Sint0
            | HvX64RegisterName::Sint1
            | HvX64RegisterName::Sint2
            | HvX64RegisterName::Sint3
            | HvX64RegisterName::Sint4
            | HvX64RegisterName::Sint5
            | HvX64RegisterName::Sint6
            | HvX64RegisterName::Sint7
            | HvX64RegisterName::Sint8
            | HvX64RegisterName::Sint9
            | HvX64RegisterName::Sint10
            | HvX64RegisterName::Sint11
            | HvX64RegisterName::Sint12
            | HvX64RegisterName::Sint13
            | HvX64RegisterName::Sint14
            | HvX64RegisterName::Sint15
            | HvX64RegisterName::Scontrol
            | HvX64RegisterName::Sversion
            | HvX64RegisterName::Sifp
            | HvX64RegisterName::Sipp
            | HvX64RegisterName::Eom
            | HvX64RegisterName::Stimer0Config
            | HvX64RegisterName::Stimer0Count
            | HvX64RegisterName::Stimer1Config
            | HvX64RegisterName::Stimer1Count
            | HvX64RegisterName::Stimer2Config
            | HvX64RegisterName::Stimer2Count
            | HvX64RegisterName::Stimer3Config
            | HvX64RegisterName::Stimer3Count
            | HvX64RegisterName::VsmVina) => self.vp.backing.cvm_state_mut().hv[vtl]
                .synic
                .read_reg(synic_reg.into()),
            HvX64RegisterName::ApicBase => Ok(self.vp.backing.cvm_state_mut().lapics[vtl]
                .lapic
                .apic_base()
                .into()),
            _ => {
                tracing::error!(
                    ?name,
                    "guest invoked getvpregister with unsupported register"
                );
                Err(HvError::InvalidParameter)
            }
        }
    }

    fn set_vp_register(
        &mut self,
        vtl: GuestVtl,
        reg: &hvdef::hypercall::HvRegisterAssoc,
    ) -> HvResult<()> {
        self.validate_register_access(vtl, reg.name)?;
        // TODO CVM:
        // - when access vp state has support for single registers, clean this
        //   up.
        // - validate the values being set, e.g. that addresses are canonical,
        //   that efer and pat make sense, etc. Similar validation is needed in
        //   the write_msr path.

        match HvX64RegisterName::from(reg.name) {
            HvX64RegisterName::VsmPartitionConfig => self.vp.set_vsm_partition_config(
                HvRegisterVsmPartitionConfig::from(reg.value.as_u64()),
                vtl,
            ),
            HvX64RegisterName::VsmVpSecureConfigVtl0 => self.vp.set_vsm_vp_secure_config_vtl(
                vtl,
                GuestVtl::Vtl0,
                HvRegisterVsmVpSecureVtlConfig::from(reg.value.as_u64()),
            ),
            HvX64RegisterName::VpAssistPage => self.vp.backing.cvm_state_mut().hv[vtl]
                .msr_write_vp_assist_page(reg.value.as_u64())
                .map_err(|_| HvError::InvalidRegisterValue),
            virt_msr @ (HvX64RegisterName::Star
            | HvX64RegisterName::Cstar
            | HvX64RegisterName::Lstar
            | HvX64RegisterName::SysenterCs
            | HvX64RegisterName::SysenterEip
            | HvX64RegisterName::SysenterEsp
            | HvX64RegisterName::Sfmask) => {
                let mut msrs = self
                    .vp
                    .access_state(vtl.into())
                    .virtual_msrs()
                    .map_err(Self::reg_access_error_to_hv_err)?;
                match virt_msr {
                    HvX64RegisterName::Star => msrs.star = reg.value.as_u64(),
                    HvX64RegisterName::Cstar => msrs.cstar = reg.value.as_u64(),
                    HvX64RegisterName::Lstar => msrs.lstar = reg.value.as_u64(),
                    HvX64RegisterName::SysenterCs => msrs.sysenter_cs = reg.value.as_u64(),
                    HvX64RegisterName::SysenterEip => msrs.sysenter_eip = reg.value.as_u64(),
                    HvX64RegisterName::SysenterEsp => msrs.sysenter_esp = reg.value.as_u64(),
                    HvX64RegisterName::Sfmask => msrs.sfmask = reg.value.as_u64(),
                    _ => unreachable!(),
                }
                self.vp
                    .access_state(vtl.into())
                    .set_virtual_msrs(&msrs)
                    .map_err(Self::reg_access_error_to_hv_err)
            }
            HvX64RegisterName::TscAux => self
                .vp
                .access_state(vtl.into())
                .set_tsc_aux(&virt::vp::TscAux {
                    value: reg.value.as_u64(),
                })
                .map_err(Self::reg_access_error_to_hv_err),

            debug_reg @ (HvX64RegisterName::Dr3 | HvX64RegisterName::Dr7) => {
                let mut debug_registers = self
                    .vp
                    .access_state(vtl.into())
                    .debug_regs()
                    .map_err(Self::reg_access_error_to_hv_err)?;
                match debug_reg {
                    HvX64RegisterName::Dr3 => debug_registers.dr3 = reg.value.as_u64(),
                    HvX64RegisterName::Dr7 => debug_registers.dr7 = reg.value.as_u64(),
                    _ => unreachable!(),
                }

                self.vp
                    .access_state(vtl.into())
                    .set_debug_regs(&debug_registers)
                    .map_err(Self::reg_access_error_to_hv_err)
            }
            HvX64RegisterName::Pat => self
                .vp
                .access_state(vtl.into())
                .set_pat(&virt::vp::Pat {
                    value: reg.value.as_u64(),
                })
                .map_err(Self::reg_access_error_to_hv_err),
            register @ (HvX64RegisterName::Efer
            | HvX64RegisterName::Cr0
            | HvX64RegisterName::Cr4
            | HvX64RegisterName::Cr8
            | HvX64RegisterName::Ldtr
            | HvX64RegisterName::Gdtr
            | HvX64RegisterName::Idtr
            | HvX64RegisterName::Rip
            | HvX64RegisterName::Rflags
            | HvX64RegisterName::Rsp) => {
                let mut registers = self
                    .vp
                    .access_state(vtl.into())
                    .registers()
                    .map_err(Self::reg_access_error_to_hv_err)?;
                match register {
                    HvX64RegisterName::Efer => registers.efer = reg.value.as_u64(),
                    HvX64RegisterName::Cr0 => registers.cr0 = reg.value.as_u64(),
                    HvX64RegisterName::Cr4 => registers.cr4 = reg.value.as_u64(),
                    HvX64RegisterName::Cr8 => registers.cr8 = reg.value.as_u64(),
                    HvX64RegisterName::Ldtr => {
                        registers.ldtr = hvdef::HvX64SegmentRegister::from(reg.value).into()
                    }
                    HvX64RegisterName::Gdtr => {
                        registers.gdtr = hvdef::HvX64TableRegister::from(reg.value).into()
                    }
                    HvX64RegisterName::Idtr => {
                        registers.idtr = hvdef::HvX64TableRegister::from(reg.value).into()
                    }
                    HvX64RegisterName::Rip => registers.rip = reg.value.as_u64(),
                    HvX64RegisterName::Rflags => registers.rflags = reg.value.as_u64(),
                    HvX64RegisterName::Rsp => registers.rsp = reg.value.as_u64(),
                    _ => unreachable!(),
                }
                self.vp
                    .access_state(vtl.into())
                    .set_registers(&registers)
                    .map_err(Self::reg_access_error_to_hv_err)
            }
            synic_reg @ (HvX64RegisterName::Sint0
            | HvX64RegisterName::Sint1
            | HvX64RegisterName::Sint2
            | HvX64RegisterName::Sint3
            | HvX64RegisterName::Sint4
            | HvX64RegisterName::Sint5
            | HvX64RegisterName::Sint6
            | HvX64RegisterName::Sint7
            | HvX64RegisterName::Sint8
            | HvX64RegisterName::Sint9
            | HvX64RegisterName::Sint10
            | HvX64RegisterName::Sint11
            | HvX64RegisterName::Sint12
            | HvX64RegisterName::Sint13
            | HvX64RegisterName::Sint14
            | HvX64RegisterName::Sint15
            | HvX64RegisterName::Scontrol
            | HvX64RegisterName::Sversion
            | HvX64RegisterName::Sifp
            | HvX64RegisterName::Sipp
            | HvX64RegisterName::Eom
            | HvX64RegisterName::Stimer0Config
            | HvX64RegisterName::Stimer0Count
            | HvX64RegisterName::Stimer1Config
            | HvX64RegisterName::Stimer1Count
            | HvX64RegisterName::Stimer2Config
            | HvX64RegisterName::Stimer2Count
            | HvX64RegisterName::Stimer3Config
            | HvX64RegisterName::Stimer3Count
            | HvX64RegisterName::VsmVina) => self.vp.backing.cvm_state_mut().hv[vtl]
                .synic
                .write_reg(&self.vp.partition.gm[vtl], synic_reg.into(), reg.value),
            HvX64RegisterName::ApicBase => {
                // No changes are allowed on this path.
                let current = self.vp.backing.cvm_state_mut().lapics[vtl]
                    .lapic
                    .apic_base();
                if reg.value.as_u64() != current {
                    return Err(HvError::InvalidParameter);
                }
                Ok(())
            }
            _ => {
                tracing::error!(
                    ?reg,
                    "guest invoked SetVpRegisters with unsupported register",
                );
                Err(HvError::InvalidParameter)
            }
        }

        // TODO GUEST VSM: interrupt rewinding
    }
}

impl<T: CpuIo, B: HardwareIsolatedBacking> hv1_hypercall::ModifySparseGpaPageHostVisibility
    for UhHypercallHandler<'_, '_, T, B>
{
    fn modify_gpa_visibility(
        &mut self,
        partition_id: u64,
        visibility: HostVisibilityType,
        gpa_pages: &[u64],
    ) -> HvRepResult {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err((HvError::AccessDenied, 0));
        }

        tracing::debug!(
            ?visibility,
            pages = gpa_pages.len(),
            "modify_gpa_visibility"
        );

        if self.vp.cvm_partition().hide_isolation {
            return Err((HvError::AccessDenied, 0));
        }

        let shared = match visibility {
            HostVisibilityType::PRIVATE => false,
            HostVisibilityType::SHARED => true,
            _ => return Err((HvError::InvalidParameter, 0)),
        };

        self.vp
            .cvm_partition()
            .isolated_memory_protector
            .change_host_visibility(shared, gpa_pages, &mut self.vp.tlb_flush_lock_access())
    }
}

impl<T: CpuIo, B: HardwareIsolatedBacking> UhHypercallHandler<'_, '_, T, B> {
    fn retarget_physical_interrupt(
        &mut self,
        device_id: u64,
        address: u64,
        data: u32,
        vector: u32,
        multicast: bool,
        target_processors: ProcessorSet<'_>,
    ) -> HvResult<()> {
        // Before dispatching retarget_device_interrupt, add the device vector
        // to partition global device vector table and issue `proxy_irr_blocked`
        // filter wake request to other VPs
        self.vp.partition.request_proxy_irr_filter_update(
            self.intercepted_vtl,
            vector as u8,
            self.vp.vp_index(),
        );

        // Update `proxy_irr_blocked` for this VP itself
        self.vp.update_proxy_irr_filter(self.intercepted_vtl);

        self.vp.partition.hcl.retarget_device_interrupt(
            device_id,
            hvdef::hypercall::InterruptEntry {
                source: hvdef::hypercall::HvInterruptSource::MSI,
                rsvd: 0,
                data: [address as u32, data],
            },
            vector,
            multicast,
            target_processors,
        )
    }

    pub(crate) fn hcvm_validate_flush_inputs(
        &mut self,
        processor_set: ProcessorSet<'_>,
        flags: HvFlushFlags,
        allow_extended_ranges: bool,
    ) -> HvResult<()> {
        let valid_flags = HvFlushFlags::new()
            .with_all_processors(true)
            .with_all_virtual_address_spaces(true)
            .with_non_global_mappings_only(true)
            .with_use_extended_range_format(allow_extended_ranges);

        if u64::from(flags) & !u64::from(valid_flags) != 0 {
            return Err(HvError::InvalidParameter);
        }
        if processor_set.is_empty() && !flags.all_processors() {
            return Err(HvError::InvalidParameter);
        }
        // TODO should we check the all_virtual_address_spaces flag? we don't check this flag or the address space input arg anywhere in the hcl
        Ok(())
    }
}

impl<T, B: HardwareIsolatedBacking> hv1_hypercall::GetVpRegisters
    for UhHypercallHandler<'_, '_, T, B>
{
    fn get_vp_registers(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        vtl: Option<Vtl>,
        registers: &[hvdef::HvRegisterName],
        output: &mut [hvdef::HvRegisterValue],
    ) -> HvRepResult {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err((HvError::AccessDenied, 0));
        }

        if vp_index != hvdef::HV_VP_INDEX_SELF && vp_index != self.vp.vp_index().index() {
            return Err((HvError::AccessDenied, 0));
        }

        let vtl = self
            .target_vtl_no_higher(vtl.unwrap_or_else(|| self.intercepted_vtl.into()))
            .map_err(|e| (e, 0))?;

        for (i, (&name, output)) in zip(registers, output).enumerate() {
            *output = self.get_vp_register(vtl, name).map_err(|e| (e, i))?;
        }

        Ok(())
    }
}

impl<T: CpuIo, B: HardwareIsolatedBacking> hv1_hypercall::RetargetDeviceInterrupt
    for UhHypercallHandler<'_, '_, T, B>
{
    fn retarget_interrupt(
        &mut self,
        device_id: u64,
        address: u64,
        data: u32,
        params: hv1_hypercall::HvInterruptParameters<'_>,
    ) -> HvResult<()> {
        let hv1_hypercall::HvInterruptParameters {
            vector,
            multicast,
            target_processors,
        } = params;
        // It is unknown whether the interrupt is physical or virtual, so try both. Note that the
        // actual response from the hypervisor can't really be trusted so:
        // 1. Always invoke the virtual interrupt retargeting.
        // 2. A failure from the physical interrupt retargeting is not necessarily a sign of a
        // malicious hypervisor or a buggy guest, since the target could simply be a virtual one.
        let hv_result = self.retarget_physical_interrupt(
            device_id,
            address,
            data,
            vector,
            multicast,
            target_processors,
        );
        let virtual_result = self.retarget_virtual_interrupt(
            device_id,
            address,
            data,
            vector,
            multicast,
            target_processors,
        );
        hv_result.or(virtual_result)
    }
}

impl<T, B: HardwareIsolatedBacking> hv1_hypercall::SetVpRegisters
    for UhHypercallHandler<'_, '_, T, B>
{
    fn set_vp_registers(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        vtl: Option<Vtl>,
        registers: &[hvdef::hypercall::HvRegisterAssoc],
    ) -> HvRepResult {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err((HvError::AccessDenied, 0));
        }

        if vp_index != hvdef::HV_VP_INDEX_SELF && vp_index != self.vp.vp_index().index() {
            return Err((HvError::InvalidVpIndex, 0));
        }

        let target_vtl = vtl
            .map_or_else(|| Ok(self.intercepted_vtl), |vtl| vtl.try_into())
            .map_err(|_| (HvError::InvalidParameter, 0))?;

        for (i, reg) in registers.iter().enumerate() {
            self.set_vp_register(target_vtl, reg).map_err(|e| (e, i))?;
        }

        Ok(())
    }
}

impl<T, B: HardwareIsolatedBacking> hv1_hypercall::VtlCall for UhHypercallHandler<'_, '_, T, B> {
    fn is_vtl_call_allowed(&self) -> bool {
        tracing::trace!("checking if vtl call is allowed");

        // Only allowed from VTL 0
        if self.intercepted_vtl != GuestVtl::Vtl0 {
            tracelimit::warn_ratelimited!(
                "vtl call not allowed from vtl {:?}",
                self.intercepted_vtl
            );
            false
        } else if !self.vp.backing.cvm_state().vtl1_enabled {
            // VTL 1 must be active on the vp
            tracelimit::warn_ratelimited!("vtl call not allowed because vtl 1 is not enabled");
            false
        } else {
            true
        }
    }

    fn vtl_call(&mut self) {
        tracing::trace!("handling vtl call");
        self.vp
            .raise_vtl(
                self.intercepted_vtl,
                GuestVtl::Vtl1,
                HvVtlEntryReason::VTL_CALL,
            )
            .unwrap();
    }
}

impl<T, B: HardwareIsolatedBacking> hv1_hypercall::VtlReturn for UhHypercallHandler<'_, '_, T, B> {
    fn is_vtl_return_allowed(&self) -> bool {
        tracing::trace!("checking if vtl return is allowed");

        if self.intercepted_vtl != GuestVtl::Vtl1 {
            tracelimit::warn_ratelimited!(
                "vtl return not allowed from vtl {:?}",
                self.intercepted_vtl
            );
        }

        // Only allowed from VTL 1
        self.intercepted_vtl != GuestVtl::Vtl0
    }

    fn vtl_return(&mut self, fast: bool) {
        tracing::trace!("handling vtl return");

        self.vp.unlock_tlb_lock(Vtl::Vtl1);

        let hv = &mut self.vp.backing.cvm_state_mut().hv[GuestVtl::Vtl1];
        if hv.synic.vina().auto_reset() {
            hv.set_vina_asserted(false).unwrap();
        }

        B::switch_vtl(self.vp, self.intercepted_vtl, GuestVtl::Vtl0);

        // TODO CVM GUEST_VSM:
        // - rewind interrupts

        if !fast {
            let [rax, rcx] = self.vp.backing.cvm_state_mut().hv[GuestVtl::Vtl1]
                .return_registers()
                .expect("getting return registers shouldn't fail");
            let mut vp_state = self.vp.access_state(Vtl::Vtl0);
            let mut registers = vp_state
                .registers()
                .expect("getting registers shouldn't fail");
            registers.rax = rax;
            registers.rcx = rcx;

            vp_state
                .set_registers(&registers)
                .expect("setting registers shouldn't fail");
        }
    }
}

impl<T, B: HardwareIsolatedBacking>
    hv1_hypercall::StartVirtualProcessor<hvdef::hypercall::InitialVpContextX64>
    for UhHypercallHandler<'_, '_, T, B>
{
    fn start_virtual_processor(
        &mut self,
        partition_id: u64,
        target_vp: u32,
        target_vtl: Vtl,
        vp_context: &hvdef::hypercall::InitialVpContextX64,
    ) -> HvResult<()> {
        tracing::debug!(
            vp_index = self.vp.vp_index().index(),
            target_vp,
            ?target_vtl,
            "HvStartVirtualProcessor"
        );

        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err(HvError::InvalidPartitionId);
        }

        if target_vp == self.vp.vp_index().index()
            || target_vp as usize >= self.vp.partition.vps.len()
        {
            return Err(HvError::InvalidVpIndex);
        }

        let target_vtl = self.target_vtl_no_higher(target_vtl)?;
        let target_vp_inner = self.vp.cvm_partition().vp_inner(target_vp);

        // The target VTL must have been enabled.
        if target_vtl == GuestVtl::Vtl1 && !*target_vp_inner.vtl1_enable_called.lock() {
            return Err(HvError::InvalidVpState);
        }

        // If lower VTL startup has been suppressed, then the request must be
        // coming from a secure VTL. We know guest VSM has been enabled from the
        // previous check.
        if self.intercepted_vtl == GuestVtl::Vtl0
            && self.vp.cvm_partition().is_lower_vtl_startup_denied()
        {
            return Err(HvError::AccessDenied);
        }

        // The StartVp hypercall is intended to work like an INIT, so it
        // theoretically can be called on an already running VP. However, this
        // makes it more difficult to reason about how to interact with higher
        // vtls and with the DenyLowerVtlStartup, and in practice, it's not clear
        // whether any guest OS does this. For now, if guest vsm is enabled,
        // simplify by disallowing repeated vp startup. Revisit this later if it
        // becomes a problem. Note that this will not apply to non-hardware cvms
        // as this may regress existing VMs.

        // After this check, there can be no more failures, so try setting the
        // fact that the VM started to true here.
        if target_vp_inner
            .started
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_err()
        {
            return Err(HvError::InvalidVpState);
        }

        let start_state = VpStartEnableVtl {
            operation: InitialVpContextOperation::StartVp,
            context: *vp_context,
        };

        *self
            .vp
            .cvm_partition()
            .vp_inner(target_vp)
            .hv_start_enable_vtl_vp[target_vtl]
            .lock() = Some(Box::new(start_state));
        self.vp.partition.vps[target_vp as usize]
            .wake(target_vtl, WakeReason::HV_START_ENABLE_VP_VTL);

        Ok(())
    }
}

impl<T, B: HardwareIsolatedBacking> hv1_hypercall::ModifyVtlProtectionMask
    for UhHypercallHandler<'_, '_, T, B>
{
    fn modify_vtl_protection_mask(
        &mut self,
        partition_id: u64,
        map_flags: HvMapGpaFlags,
        target_vtl: Option<Vtl>,
        gpa_pages: &[u64],
    ) -> HvRepResult {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err((HvError::AccessDenied, 0));
        }

        let target_vtl = self
            .target_vtl_no_higher(target_vtl.unwrap_or(self.intercepted_vtl.into()))
            .map_err(|e| (e, 0))?;
        if target_vtl == GuestVtl::Vtl0 {
            return Err((HvError::InvalidParameter, 0));
        }

        let protector = &self.vp.cvm_partition().isolated_memory_protector;

        // A VTL cannot change its own VTL permissions until it has enabled VTL protection and
        // configured default permissions. Higher VTLs are not under this restriction (as they may
        // need to apply default permissions before VTL protection is enabled).
        if target_vtl == self.intercepted_vtl && !protector.vtl1_protections_enabled() {
            return Err((HvError::AccessDenied, 0));
        }

        // VTL 1 must be enabled already.
        let guest_vsm_lock = self.vp.cvm_partition().guest_vsm.read();
        let GuestVsmState::Enabled { vtl1, .. } = &*guest_vsm_lock else {
            return Err((HvError::InvalidVtlState, 0));
        };

        if !validate_vtl_gpa_flags(
            map_flags,
            vtl1.mbec_enabled,
            vtl1.shadow_supervisor_stack_enabled,
        ) {
            return Err((HvError::InvalidRegisterValue, 0));
        }

        // The contract for VSM is that the VTL protections describe what
        // the lower VTLs are allowed to access. Hardware CVMs set the
        // protections on the VTL itself. Therefore, for a hardware CVM,
        // given that only VTL 1 can set the protections, the default
        // permissions should be changed for VTL 0.
        protector.change_vtl_protections(
            GuestVtl::Vtl0,
            gpa_pages,
            map_flags,
            &mut self.vp.tlb_flush_lock_access(),
        )
    }
}

impl<T: CpuIo, B: HardwareIsolatedBacking> hv1_hypercall::QuerySparseGpaPageHostVisibility
    for UhHypercallHandler<'_, '_, T, B>
{
    fn query_gpa_visibility(
        &mut self,
        partition_id: u64,
        gpa_pages: &[u64],
        host_visibility: &mut [HostVisibilityType],
    ) -> HvRepResult {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err((HvError::AccessDenied, 0));
        }

        if self.vp.cvm_partition().hide_isolation {
            return Err((HvError::AccessDenied, 0));
        }

        self.vp
            .cvm_partition()
            .isolated_memory_protector
            .query_host_visibility(gpa_pages, host_visibility)
    }
}

impl<T, B: HardwareIsolatedBacking> hv1_hypercall::EnablePartitionVtl
    for UhHypercallHandler<'_, '_, T, B>
{
    fn enable_partition_vtl(
        &mut self,
        partition_id: u64,
        target_vtl: Vtl,
        flags: hvdef::hypercall::EnablePartitionVtlFlags,
    ) -> HvResult<()> {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err(HvError::InvalidPartitionId);
        }

        let target_vtl = GuestVtl::try_from(target_vtl).map_err(|_| HvError::AccessDenied)?;
        if target_vtl != GuestVtl::Vtl1 {
            return Err(HvError::AccessDenied);
        }

        if flags.enable_supervisor_shadow_stack() || flags.enable_hardware_hvpt() {
            return Err(HvError::InvalidParameter);
        }

        let mut gvsm_state = self.vp.cvm_partition().guest_vsm.write();

        match *gvsm_state {
            GuestVsmState::NotPlatformSupported => return Err(HvError::AccessDenied),
            GuestVsmState::NotGuestEnabled => (),
            GuestVsmState::Enabled { vtl1: _ } => {
                // VTL 1 cannot be already enabled
                return Err(HvError::VtlAlreadyEnabled);
            }
        }

        self.vp.partition.hcl.enable_partition_vtl(
            target_vtl,
            // These flags are managed and enforced internally; CVMs can't rely
            // on the hypervisor
            0.into(),
        )?;

        *gvsm_state = GuestVsmState::Enabled {
            vtl1: CvmVtl1State {
                mbec_enabled: flags.enable_mbec(),
                ..Default::default()
            },
        };

        let protector = &self.vp.cvm_partition().isolated_memory_protector;

        // Grant VTL 1 access to lower VTL memory
        tracing::debug!("Granting VTL 1 access to lower VTL memory");
        protector.change_default_vtl_protections(
            GuestVtl::Vtl1,
            hvdef::HV_MAP_GPA_PERMISSIONS_ALL,
            &mut self.vp.tlb_flush_lock_access(),
        )?;

        tracing::debug!("Successfully granted vtl 1 access to lower vtl memory");

        tracing::info!("Enabled vtl 1 on the partition");

        Ok(())
    }
}

impl<T, B: HardwareIsolatedBacking>
    hv1_hypercall::EnableVpVtl<hvdef::hypercall::InitialVpContextX64>
    for UhHypercallHandler<'_, '_, T, B>
{
    fn enable_vp_vtl(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        vtl: Vtl,
        vp_context: &hvdef::hypercall::InitialVpContextX64,
    ) -> HvResult<()> {
        tracing::debug!(
            vp_index = self.vp.vp_index().index(),
            target_vp = vp_index,
            ?vtl,
            "HvEnableVpVtl"
        );
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err(HvError::InvalidPartitionId);
        }

        if vp_index as usize >= self.vp.partition.vps.len() {
            return Err(HvError::InvalidVpIndex);
        }

        let vtl = GuestVtl::try_from(vtl).map_err(|_| HvError::InvalidParameter)?;
        if vtl != GuestVtl::Vtl1 {
            return Err(HvError::InvalidParameter);
        }

        // If handling on behalf of VTL 0, then lock to make sure that no other
        // VP makes this call on behalf of VTL 0.
        let gvsm_state = {
            let guest_vsm_lock = self.vp.cvm_partition().guest_vsm.write();

            // Should be enabled on the partition
            let vtl1 = parking_lot::RwLockWriteGuard::try_map(guest_vsm_lock, |gvsm| {
                if let GuestVsmState::Enabled { vtl1, .. } = &mut *gvsm {
                    Some(vtl1)
                } else {
                    None
                }
            })
            .map_err(|_| HvError::InvalidVtlState)?;

            let current_vp_index = self.vp.vp_index().index();

            // A higher VTL can only be enabled on the current processor to make
            // sure that the lower VTL is executing at a known point, and only if
            // the higher VTL has not been enabled on any other VP because at that
            // point, the higher VTL should be orchestrating its own enablement.
            if self.intercepted_vtl < GuestVtl::Vtl1 {
                if vtl1.enabled_on_any_vp || vp_index != current_vp_index {
                    return Err(HvError::AccessDenied);
                }

                Some(vtl1)
            } else {
                // If handling on behalf of VTL 1, then some other VP (i.e. the
                // bsp) must have already handled EnableVpVtl. No partition-wide
                // state is changing, so no need to hold the lock
                assert!(vtl1.enabled_on_any_vp);
                None
            }
        };

        // Lock the remote vp state to make sure no other VP is trying to enable
        // VTL 1 on it.
        let mut vtl1_enabled = self
            .vp
            .cvm_partition()
            .vp_inner(vp_index)
            .vtl1_enable_called
            .lock();

        if *vtl1_enabled {
            return Err(HvError::VtlAlreadyEnabled);
        }

        let hv_vp_context = match self.vp.partition.isolation {
            virt::IsolationType::None | virt::IsolationType::Vbs => unreachable!(),
            virt::IsolationType::Snp => {
                // For VTL 1, user mode needs to explicitly register the VMSA
                // with the hypervisor via the EnableVpVtl hypercall.
                let vmsa_pfn = self.vp.partition.hcl.vtl1_vmsa_pfn(vp_index);
                let sev_control = hvdef::HvX64RegisterSevControl::new()
                    .with_enable_encrypted_state(true)
                    .with_vmsa_gpa_page_number(vmsa_pfn);

                let mut hv_vp_context = hvdef::hypercall::InitialVpContextX64::new_zeroed();
                hv_vp_context.rip = sev_control.into();

                hv_vp_context
            }
            virt::IsolationType::Tdx => hvdef::hypercall::InitialVpContextX64::new_zeroed(),
        };

        // Tell the hypervisor to enable VTL 1, and register any needed state
        self.vp
            .partition
            .hcl
            .enable_vp_vtl(vp_index, vtl, hv_vp_context)?;

        // Cannot fail from here
        if let Some(mut vtl1) = gvsm_state {
            // It's valid to only set this when gvsm_state is Some (when VTL 0
            // was intercepted) only because we assert above that if VTL 1 was
            // intercepted, some vp has already enabled VTL 1 on it.
            vtl1.enabled_on_any_vp = true;
        }

        *vtl1_enabled = true;

        let enable_vp_vtl_state = VpStartEnableVtl {
            operation: InitialVpContextOperation::EnableVpVtl,
            context: *vp_context,
        };

        *self
            .vp
            .cvm_partition()
            .vp_inner(vp_index)
            .hv_start_enable_vtl_vp[vtl]
            .lock() = Some(Box::new(enable_vp_vtl_state));
        self.vp.partition.vps[vp_index as usize].wake(vtl, WakeReason::HV_START_ENABLE_VP_VTL);

        tracing::debug!(vp_index, "enabled vtl 1 on vp");

        Ok(())
    }
}

impl<T, B: HardwareIsolatedBacking> hv1_hypercall::TranslateVirtualAddressX64
    for UhHypercallHandler<'_, '_, T, B>
{
    fn translate_virtual_address(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        control_flags: hvdef::hypercall::TranslateGvaControlFlagsX64,
        gva_page: u64,
    ) -> HvResult<hvdef::hypercall::TranslateVirtualAddressOutput> {
        if partition_id != hvdef::HV_PARTITION_ID_SELF {
            return Err(HvError::AccessDenied);
        }

        if vp_index != hvdef::HV_VP_INDEX_SELF && vp_index != self.vp.vp_index().index() {
            return Err(HvError::AccessDenied);
        }

        let target_vtl = self
            .target_vtl_no_higher(
                control_flags
                    .input_vtl()
                    .target_vtl()?
                    .unwrap_or(self.intercepted_vtl.into()),
            )
            .map_err(|_| HvError::AccessDenied)?;

        if self.intercepted_vtl == target_vtl {
            return Err(HvError::AccessDenied);
        }

        let gva = gva_page * hvdef::HV_PAGE_SIZE;

        if control_flags.tlb_flush_inhibit() {
            self.vp
                .set_tlb_lock(self.intercepted_vtl.into(), target_vtl);
        }

        match virt_support_x86emu::translate::translate_gva_to_gpa(
            &self.vp.partition.gm[target_vtl], // TODO GUEST VSM: This doesn't have VTL access checks.
            gva,
            &self.vp.backing.translation_registers(self.vp, target_vtl),
            virt_support_x86emu::translate::TranslateFlags::from_hv_flags(control_flags),
        ) {
            Ok(virt_support_x86emu::translate::TranslateResult { gpa, cache_info }) => {
                // TODO GUEST VSM: at the moment, the guest is only using this
                // to check for overlay pages related to drivers and executable
                // code. Only the hypercall code page overlay matches that
                // description. However, for full correctness this should be
                // extended to check for all overlay pages.
                let overlay_page = hvdef::hypercall::MsrHypercallContents::from(
                    self.vp.backing.cvm_state_mut().hv[target_vtl]
                        .msr_read(hvdef::HV_X64_MSR_HYPERCALL)
                        .unwrap(),
                )
                .gpn();

                let cache_type = match cache_info {
                    TranslateCachingInfo::NoPaging => HvCacheType::HvCacheTypeWriteBack.0 as u8,
                    TranslateCachingInfo::Paging { pat_index } => {
                        ((self.vp.access_state(target_vtl.into()).pat().unwrap().value
                            >> (pat_index * 8))
                            & 0xff) as u8
                    }
                };

                let gpn = gpa / hvdef::HV_PAGE_SIZE;
                Ok(hvdef::hypercall::TranslateVirtualAddressOutput {
                    translation_result: hvdef::hypercall::TranslateGvaResult::new()
                        .with_result_code(TranslateGvaResultCode::SUCCESS.0)
                        .with_overlay_page(gpn == overlay_page)
                        .with_cache_type(cache_type),
                    gpa_page: gpn,
                })
            }
            Err(err) => Ok(hvdef::hypercall::TranslateVirtualAddressOutput {
                translation_result: hvdef::hypercall::TranslateGvaResult::new()
                    .with_result_code(TranslateGvaResultCode::from(err).0),
                gpa_page: 0,
            }),
        }
    }
}

struct HypercallOverlayAccess<'a> {
    vtl: GuestVtl,
    protector: &'a dyn crate::ProtectIsolatedMemory,
    tlb_access: &'a mut dyn TlbFlushLockAccess,
}

impl hv1_emulator::hv::VtlProtectHypercallOverlay for HypercallOverlayAccess<'_> {
    fn change_overlay(&mut self, gpn: u64) {
        self.protector
            .change_hypercall_overlay(self.vtl, gpn, self.tlb_access)
    }

    fn disable_overlay(&mut self) {
        self.protector
            .disable_hypercall_overlay(self.vtl, self.tlb_access)
    }
}

#[expect(private_bounds)]
impl<B: HardwareIsolatedBacking> UhProcessor<'_, B> {
    pub(crate) fn write_msr_cvm(
        &mut self,
        msr: u32,
        value: u64,
        vtl: GuestVtl,
    ) -> Result<(), MsrError> {
        let self_index = self.vp_index();
        let hv = &mut self.backing.cvm_state_mut().hv[vtl];
        // If updated is Synic MSR, then check if its proxy or previous was proxy
        // in either case, we need to update the `proxy_irr_blocked`
        let mut irr_filter_update = false;
        if matches!(msr, hvdef::HV_X64_MSR_SINT0..=hvdef::HV_X64_MSR_SINT15) {
            let sint_curr = HvSynicSint::from(hv.synic.sint((msr - hvdef::HV_X64_MSR_SINT0) as u8));
            let sint_new = HvSynicSint::from(value);
            if sint_curr.proxy() || sint_new.proxy() {
                irr_filter_update = true;
            }
        }

        // Perform this delay dance with the TLB flush to avoid a double borrow.
        // We need the whole UhProcessor to perform a flush, but the hv emulator
        // is inside the UhProcessor.
        let mut access = HypercallOverlayAccess {
            vtl,
            protector: B::cvm_partition_state(self.shared)
                .isolated_memory_protector
                .as_ref(),
            // Don't call the helper method, break out into partial borrows so we
            // can interact with the hv at the same time.
            tlb_access: &mut B::tlb_flush_lock_access(self_index, self.partition, self.shared),
        };
        let r = hv.msr_write(msr, value, &mut access);

        if !matches!(r, Err(MsrError::Unknown)) {
            // Check if proxy filter update was required (in case of SINT writes)
            if irr_filter_update {
                self.update_proxy_irr_filter(vtl);
            }
        }
        r
    }

    fn set_vsm_partition_config(
        &mut self,
        value: HvRegisterVsmPartitionConfig,
        vtl: GuestVtl,
    ) -> Result<(), HvError> {
        if vtl != GuestVtl::Vtl1 {
            return Err(HvError::InvalidParameter);
        }

        assert!(self.partition.isolation.is_isolated());

        // Features currently supported by openhcl.
        let allowed_bits = HvRegisterVsmPartitionConfig::new()
            .with_enable_vtl_protection(true)
            .with_default_vtl_protection_mask(0xf)
            .with_zero_memory_on_reset(true)
            .with_deny_lower_vtl_startup(true);

        if (!u64::from(allowed_bits) & u64::from(value)) != 0 {
            return Err(HvError::InvalidRegisterValue);
        }

        // VTL 1 must be enabled already.
        let mut guest_vsm_lock = self.cvm_partition().guest_vsm.write();
        let GuestVsmState::Enabled { vtl1, .. } = &mut *guest_vsm_lock else {
            return Err(HvError::InvalidVtlState);
        };

        let protections = HvMapGpaFlags::from(value.default_vtl_protection_mask() as u32);

        let protector = &self.cvm_partition().isolated_memory_protector;
        // VTL protection cannot be disabled once enabled.
        if !value.enable_vtl_protection() && protector.vtl1_protections_enabled() {
            return Err(HvError::InvalidRegisterValue);
        }

        if !validate_vtl_gpa_flags(
            protections,
            vtl1.mbec_enabled,
            vtl1.shadow_supervisor_stack_enabled,
        ) {
            return Err(HvError::InvalidRegisterValue);
        }

        // Default VTL protection mask must include read and write.
        if !(protections.readable() && protections.writable()) {
            return Err(HvError::InvalidRegisterValue);
        }

        // Protections given to set_vsm_partition_config actually apply to VTLs lower
        // than the VTL specified as an argument for hardware CVMs.
        let targeted_vtl = GuestVtl::Vtl0;

        // Don't allow changing existing protections once vtl protection is enabled
        if protector.vtl1_protections_enabled() {
            let current_protections = protector.default_vtl0_protections();
            if protections != current_protections {
                return Err(HvError::InvalidRegisterValue);
            }
        }

        protector.change_default_vtl_protections(
            targeted_vtl,
            protections,
            &mut self.tlb_flush_lock_access(),
        )?;

        // TODO GUEST VSM: actually use the enable_vtl_protection value when
        // deciding whether to check vtl access()
        // TODO GUEST VSM: should only be set if enable_vtl_protection is true?
        protector.set_vtl1_protections_enabled();

        // Note: Zero memory on reset will happen regardless of this value,
        // since reset that involves resetting from UEFI isn't supported, and
        // the partition will get torn down and reconstructed by the host.
        vtl1.zero_memory_on_reset = value.zero_memory_on_reset();
        vtl1.deny_lower_vtl_startup = value.deny_lower_vtl_startup();

        Ok(())
    }

    /// Returns the partition-wide CVM state.
    pub(crate) fn cvm_partition(&self) -> &'_ crate::UhCvmPartitionState {
        B::cvm_partition_state(self.shared)
    }

    /// Returns the per-vp cvm inner state for this vp
    pub(crate) fn cvm_vp_inner(&self) -> &'_ crate::UhCvmVpInner {
        self.cvm_partition().vp_inner(self.vp_index().index())
    }

    /// Returns the appropriately backed TLB flush and lock access
    pub(crate) fn tlb_flush_lock_access(&self) -> impl TlbFlushLockAccess + use<'_, B> {
        B::tlb_flush_lock_access(self.vp_index(), self.partition, self.shared)
    }

    /// Handle checking for cross-VTL interrupts, preempting VTL 0, and setting
    /// VINA when appropriate. The `is_interrupt_pending` function should return
    /// true if an interrupt of appropriate priority, or an NMI, is pending for
    /// the given VTL. The boolean specifies whether RFLAGS.IF should be checked.
    /// Returns true if interrupt reprocessing is required.
    pub(crate) fn cvm_handle_cross_vtl_interrupts(
        &mut self,
        is_interrupt_pending: impl Fn(&mut Self, GuestVtl, bool) -> bool,
    ) -> Result<bool, UhRunVpError> {
        let cvm_state = self.backing.cvm_state();

        // If VTL1 is not yet enabled, there is nothing to do.
        if !cvm_state.vtl1_enabled {
            return Ok(false);
        }

        // Check for VTL preemption - which ignores RFLAGS.IF
        if cvm_state.exit_vtl == GuestVtl::Vtl0 && is_interrupt_pending(self, GuestVtl::Vtl1, false)
        {
            self.raise_vtl(GuestVtl::Vtl0, GuestVtl::Vtl1, HvVtlEntryReason::INTERRUPT)?;
        }

        let mut reprocessing_required = false;

        // Check for VINA
        if self.backing.cvm_state().exit_vtl == GuestVtl::Vtl1
            && is_interrupt_pending(self, GuestVtl::Vtl0, true)
        {
            let hv = &self.backing.cvm_state().hv[GuestVtl::Vtl1];
            let vina = hv.synic.vina();

            if vina.enabled() && !hv.vina_asserted().map_err(UhRunVpError::VpAssistPage)? {
                hv.set_vina_asserted(true)
                    .map_err(UhRunVpError::VpAssistPage)?;
                self.partition
                    .synic_interrupt(self.vp_index(), GuestVtl::Vtl1)
                    .request_interrupt(vina.vector().into(), vina.auto_eoi());
                reprocessing_required = true;
            }
        }

        Ok(reprocessing_required)
    }

    pub(crate) fn hcvm_handle_vp_start_enable_vtl(
        &mut self,
        vtl: GuestVtl,
    ) -> Result<(), UhRunVpError> {
        let context = {
            self.cvm_vp_inner().hv_start_enable_vtl_vp[vtl]
                .lock()
                .take()
        };
        if let Some(start_enable_vtl_state) = context {
            if vtl == GuestVtl::Vtl1 {
                assert!(*self.cvm_vp_inner().vtl1_enable_called.lock());
                if let InitialVpContextOperation::EnableVpVtl = start_enable_vtl_state.operation {
                    self.backing.cvm_state_mut().vtl1_enabled = true;
                }
            }

            tracing::debug!(
                vp_index = self.vp_index().index(),
                ?vtl,
                ?start_enable_vtl_state.operation,
                "setting up vp with initial registers"
            );

            hv1_emulator::hypercall::set_x86_vp_context(
                &mut self.access_state(vtl.into()),
                &(start_enable_vtl_state.context),
            )
            .map_err(UhRunVpError::State)?;

            if let InitialVpContextOperation::StartVp = start_enable_vtl_state.operation {
                match vtl {
                    GuestVtl::Vtl0 => {
                        if self.backing.cvm_state().vtl1_enabled {
                            // When starting a VP targeting VTL on a
                            // hardware confidential VM, if VTL 1 has been
                            // enabled, switch to it (the highest enabled
                            // VTL should run first). This is largely true
                            // because startvp is disallowed on a VP that
                            // has already been started. If this is allowed
                            // in the future, whether to switch to VTL 1 on
                            // a second+ startvp call for a vp should be
                            // revisited.
                            //
                            // Furthermore, there is no need to copy the
                            // shared VTL registers if starting the VP on an
                            // already running VP is disallowed. Even if
                            // this was allowed, copying the registers may
                            // not be desirable.

                            self.backing.cvm_state_mut().exit_vtl = GuestVtl::Vtl1;
                        }
                    }
                    GuestVtl::Vtl1 => {
                        self.backing.cvm_state_mut().exit_vtl = GuestVtl::Vtl1;
                    }
                }
            }
        }

        Ok(())
    }

    pub(crate) fn hcvm_vtl1_inspectable(&self) -> bool {
        self.backing.cvm_state().vtl1_enabled
    }

    fn get_vsm_vp_secure_config_vtl(
        &mut self,
        requesting_vtl: GuestVtl,
        target_vtl: GuestVtl,
    ) -> Result<HvRegisterVsmVpSecureVtlConfig, HvError> {
        if requesting_vtl <= target_vtl {
            return Err(HvError::AccessDenied);
        }

        let requesting_vtl = requesting_vtl.into();

        let guest_vsm_lock = self.cvm_partition().guest_vsm.read();
        let GuestVsmState::Enabled { vtl1, .. } = &*guest_vsm_lock else {
            return Err(HvError::InvalidVtlState);
        };

        let tlb_locked = self.vtls_tlb_locked.get(requesting_vtl, target_vtl);

        Ok(HvRegisterVsmVpSecureVtlConfig::new()
            .with_mbec_enabled(vtl1.mbec_enabled)
            .with_tlb_locked(tlb_locked))
    }

    fn set_vsm_vp_secure_config_vtl(
        &mut self,
        requesting_vtl: GuestVtl,
        target_vtl: GuestVtl,
        config: HvRegisterVsmVpSecureVtlConfig,
    ) -> Result<(), HvError> {
        tracing::debug!(
            ?requesting_vtl,
            ?target_vtl,
            "setting vsm vp secure config vtl"
        );
        if requesting_vtl <= target_vtl {
            return Err(HvError::AccessDenied);
        }

        if config.supervisor_shadow_stack_enabled() || config.hardware_hvpt_enabled() {
            return Err(HvError::InvalidRegisterValue);
        }

        let requesting_vtl = requesting_vtl.into();

        {
            let guest_vsm_lock = self.cvm_partition().guest_vsm.read();
            let GuestVsmState::Enabled { vtl1, .. } = &*guest_vsm_lock else {
                return Err(HvError::InvalidVtlState);
            };

            // MBEC must always be enabled or disabled partition-wide.
            if config.mbec_enabled() != vtl1.mbec_enabled {
                return Err(HvError::InvalidRegisterValue);
            }
        }

        let tlb_locked = self.vtls_tlb_locked.get(requesting_vtl, target_vtl);
        match (tlb_locked, config.tlb_locked()) {
            (true, false) => self.unlock_tlb_lock_target(requesting_vtl, target_vtl),
            (false, true) => self.set_tlb_lock(requesting_vtl, target_vtl),
            _ => (), // Nothing to do
        };

        Ok(())
    }

    fn raise_vtl(
        &mut self,
        source_vtl: GuestVtl,
        target_vtl: GuestVtl,
        entry_reason: HvVtlEntryReason,
    ) -> Result<(), UhRunVpError> {
        assert!(source_vtl < target_vtl);
        B::switch_vtl(self, source_vtl, target_vtl);
        self.backing.cvm_state_mut().hv[target_vtl]
            .set_return_reason(entry_reason)
            .map_err(UhRunVpError::VpAssistPage)
    }
}

pub(crate) struct XsetbvExitInput {
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub cr4: u64,
    pub cpl: u8,
}

/// Validates registers are in the correct states during a xsetbv exit, and return
/// the new xfem value if everything's valid.
pub(crate) fn validate_xsetbv_exit(input: XsetbvExitInput) -> Option<u64> {
    let XsetbvExitInput {
        rax,
        rcx,
        rdx,
        cr4,
        cpl,
    } = input;

    if rcx != 0 {
        tracelimit::warn_ratelimited!(rcx, "xsetbv exit: rcx is not set to 0");
        return None;
    }

    if cpl != 0 {
        tracelimit::warn_ratelimited!(cpl, "xsetbv exit: invalid cpl");
        return None;
    }

    let osxsave_flag = cr4 & x86defs::X64_CR4_OSXSAVE;
    if osxsave_flag == 0 {
        tracelimit::warn_ratelimited!(cr4, "xsetbv exit: cr4 osxsave not set");
        return None;
    }

    let xfem = (rdx << 32) | (rax & 0xffffffff);

    if (xfem & x86defs::xsave::XFEATURE_X87) == 0 {
        tracelimit::warn_ratelimited!(xfem, "xsetbv exit: xfem legacy x87 bit not set");
        return None;
    }

    Some(xfem)
}

impl<T: CpuIo, B: HardwareIsolatedBacking> TranslateGvaSupport for UhEmulationState<'_, '_, T, B> {
    type Error = UhRunVpError;

    fn guest_memory(&self) -> &GuestMemory {
        &self.vp.partition.gm[self.vtl]
    }

    fn acquire_tlb_lock(&mut self) {
        self.vp.set_tlb_lock(Vtl::Vtl2, self.vtl)
    }

    fn registers(&mut self) -> Result<TranslationRegisters, Self::Error> {
        Ok(self.vp.backing.translation_registers(self.vp, self.vtl))
    }
}
