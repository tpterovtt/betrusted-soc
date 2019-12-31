#![no_std]


/// efuse API for 7-series FPGAs
/// 
/// There are three fuse types to burn: USER, KEY, and CNTL
/// 
/// USER and KEY fuses share a similar ECC structure ,and in fact, the USER fuses partially
/// share a fuse bank with the KEY.
/// 
/// CNTL fuses are unique in that instead of having ECC, each fuse has two copies, and are burned
/// in duplicate for reliability. 
/// 
/// Fuses are write-once. It's also not possible within the documented command set to read out the
/// raw fuse values once burned -- they can only be implied through a set of readback calls. 
/// This means the fuse life cycle looks like this:
///   * Initial, unprogrammed factory state is all 0's
///   * USER/KEY data is coded by blowing only the 1's. An ECC code must also be blown simultaneously 
///     to match the final pattern of 1's for correct readout
///   * It seems that patches to fuses can be done, so long as it only involves changing 0->1 and results
///     in a valid state after ECC is factored in. This is especially true for data values striped across
///     multiple banks.
/// 
/// Patching support may be particularly valuable in the case that e.g. anti-rollback fusing is desired.
/// 
/// This API implements the following features:
///   * retrieve the current fuse state
///   * validate if a proposed state change results in a valid operation (only 0->1 including ECC mods)
///   * perform the actual burn operation
/// 
/// In order to represent the fusing structure more accurately, this module models the state of fuses
/// not by their logical function, but by their physical mapping into the bank. There is then a layer
/// of code that can convert the physical bank information into the logical view. Validation code thus
/// works with a set of calls that can validate bank-by-bank, which are then called by the meta-functions
/// which will implement the logical KEY/USER/CNTL requests. 
/// 

use jtag::*;
use efuse_ecc::efuse_ecc::*;

/// There are 13 banks of fuses, 12 of which (key/user) are "hamming" ECC, 1 of which (config) is "dup" ECC.
pub struct EfusePhy {
    banks: [u32; 13],
    key: [u8; 32],
    user: u32,
    cntl: u8,
}

const FUSE_BANKS: usize = 13;
const KEY_BANKS: usize = 11;
const CMD_FUSE_USER: u32 = 0b110011;
const CMD_FUSE_KEY: u32 = 0b110001;
const CMD_FUSE_CNTL: u32 = 0b110100;

impl EfusePhy {

    pub fn new() -> Self {
        EfusePhy {
            /// bank mapping as follows: 
            /// 0 - config
            /// 1-11 - key (11 shared with user LSB)
            /// 12 - user
            banks: [0; FUSE_BANKS],
            key: [0; 32],
            user: 0,
            cntl: 0,
        }
    }

    pub fn user(&self) -> u32 { self.user }
    pub fn cntl(&self) -> u8 { self.cntl }
    pub fn key(&self) -> [u8; 32] { self.key }

    /// fetch the current fuse state
    pub fn fetch<T: JtagPhy>(&mut self, jm: &mut JtagMach, jp: &mut T) {
        jm.reset(jp);

        // get the KEY fuse
        let mut ir_leg: JtagLeg = JtagLeg::new(JtagChain::IR, "cmd");
        ir_leg.push_u32(CMD_FUSE_KEY, 6, JtagEndian::Little);
        jm.add(ir_leg);
        jm.next(jp);
        assert!(jm.get().is_some());

        let mut data_leg: JtagLeg = JtagLeg::new(JtagChain::DR, "fuse");
        data_leg.push_u128(0, 128, JtagEndian::Big);
        data_leg.push_u128(0, 128, JtagEndian::Big);
        jm.add(data_leg);
        jm.next(jp);
        if let Some(mut data) = jm.get() {
            let mut bank_data: u32;
            for index in 0..KEY_BANKS {
                if index == 0 {
                    // first bank is special because it's split with the user fuse
                    bank_data = data.pop_u32(16, JtagEndian::Little).unwrap();
                    self.banks[11-index] = bank_data;
                } else {
                    bank_data = data.pop_u32(24, JtagEndian::Little).unwrap();
                    self.banks[11-index] = add_ecc(bank_data);
                }
            }
        } else {
            assert!(false);
        }
        // easiest just to re-run the command and copy it out to the u8 array
        let mut data_leg: JtagLeg = JtagLeg::new(JtagChain::DR, "fuse");
        data_leg.push_u128(0, 128, JtagEndian::Big);
        data_leg.push_u128(0, 128, JtagEndian::Big);
        jm.add(data_leg);
        jm.next(jp);
        if let Some(mut data) = jm.get() {
            for index in 0..32 {
                self.key[index] = data.pop_u8(8, JtagEndian::Little).unwrap();
            }
        } else {
            assert!(false);
        }

        // get the USER fuse and populate the split bank
        let mut ir_leg: JtagLeg = JtagLeg::new(JtagChain::IR, "cmd");
        ir_leg.push_u32(CMD_FUSE_USER, 6, JtagEndian::Little);
        jm.add(ir_leg);
        jm.next(jp);
        assert!(jm.get().is_some());

        let mut data_leg: JtagLeg = JtagLeg::new(JtagChain::DR, "user");
        data_leg.push_u32(0, 32, JtagEndian::Little);
        jm.add(data_leg);
        jm.next(jp);
        if let Some(mut data) = jm.get() {
            let user_data: u32 = data.pop_u32(32, JtagEndian::Little).unwrap();
            self.user = user_data;
            self.banks[11] |= (user_data & 0xFF) << 16;
            self.banks[11] = add_ecc(self.banks[11]);

            self.banks[12] = add_ecc( (user_data >> 8) & 0xFF_FF_FF);
        } else {
            assert!(false);
        }

        // get the CNTL fuse
        let mut ir_leg: JtagLeg = JtagLeg::new(JtagChain::IR, "cmd");
        ir_leg.push_u32(CMD_FUSE_CNTL, 6, JtagEndian::Little);
        jm.add(ir_leg);
        jm.next(jp);
        assert!(jm.get().is_some());

        let mut data_leg: JtagLeg = JtagLeg::new(JtagChain::DR, "cntl");
        data_leg.push_u32(0, 14, JtagEndian::Little); // cntl only has 14 bits length, but only bottom 6 bits are documented
        jm.add(data_leg);
        jm.next(jp);
        if let Some(mut data) = jm.get() {
            let cntl_data: u32 = data.pop_u32(14, JtagEndian::Little).unwrap();
            self.cntl = (cntl_data & 0x3F) as u8;
            self.banks[0] = cntl_data & 0x3F;
            self.banks[0] |= (cntl_data & 0x3F) << 14; // ths is the redundant value, no ECC on this bank
        } else {
            assert!(false);
        }
    }
}

pub struct EfuseApi {
    key: [u8; 32],
    user: u32,
    cntl: u8,
    phy: EfusePhy,
}

impl EfuseApi {
    pub fn new() -> Self {
        EfuseApi {
            key: [0; 32],
            user: 0,
            cntl: 0,
            phy: EfusePhy::new(),
        }
    }
    /// phy_ series of calls returns the current "phy" state, that is, the actual programmed state
    pub fn phy_key(&self) -> [u8; 32] { self.phy.key() }
    pub fn phy_user(&self) -> u32 { self.phy.user() }
    pub fn phy_cntl(&self) -> u8 { self.phy.cntl() }

    /// api_ series of call returns the current "api" state, which is the intended state to be programmed if not yet programmed
    pub fn api_key(&self) -> [u8; 32] { self.key }
    pub fn api_user(&self) -> u32 { self.user }
    pub fn api_cntl(&self) -> u8 { self.cntl }


    // synchronizes the API state with the hardware. Needs to be called first.
    pub fn fetch<T: JtagPhy>(&mut self, jm: &mut JtagMach, jp: &mut T) {
        self.phy.fetch(jm, jp);
    }

    pub fn set_key(&mut self, new_key: [u8; 32]) {
        for i in 0..32 {
            self.key[i] = new_key[i];
        }
    }
    pub fn set_user(&mut self, new_user: u32) { self.user = new_user; }
    pub fn set_cntl(&mut self, new_cntl: u8) { self.cntl = new_cntl; }

    pub fn is_valid(&mut self) -> bool {
        let mut valid: bool = true;

        // go through each bank and check if the current configuratiion only involves 0->1 flips or no change
        for index in 0..KEY_BANKS {
            if index == 0 {
                // handle cntl special case
                if ((self.phy.banks[0] & 0x3F) as u8 ^ self.cntl) & (self.phy.banks[0] & 0x3F) as u8 != 0 {
                    valid = false;
                }
            } else if index == 12 {
                // handle user special case
                if ((self.phy.banks[index] ^ add_ecc(self.user >> 8)) & self.phy.banks[index]) != 0 {
                    valid = false;
                }
            } else if index == 11 {
                // handle user + key special case
                let raw_fuse: u32 = ((self.user & 0xFF) << 16) | (self.key[31] as u32) << 8 | self.key[30] as u32;
                if ((self.phy.banks[index] ^ add_ecc(raw_fuse)) & self.phy.banks[index]) != 0 {
                    valid = false;
                }
            } else {
                // handle key fuses (most of the bank)
                let mut raw_fuse: u32 = 0;
                for i in 0..3 { 
                    raw_fuse <<= 8;
                    raw_fuse |= self.key[(index-1)*3 + 2-i] as u32; 
                }
                if ((self.phy.banks[index] ^ add_ecc(raw_fuse)) & self.phy.banks[index]) != 0 {
                    valid = false;
                }
            }
        }
        valid
    }

    fn jtag_seq<T: JtagPhy>(&mut self, jm: &mut JtagMach, jp: &mut T, cmds: &[(JtagChain, usize, u64, &str)] ) -> u128 {
        let mut ret: u128 = 0;

        for tuple in cmds.iter() {
            let (chain, count, value, comment) = *tuple;
            let mut leg: JtagLeg = JtagLeg::new(chain, comment);
            leg.push_u128(value as u128, count, JtagEndian::Little);
            jm.add(leg);
        }
        while jm.has_pending() {
            jm.next(jp);
            if let Some(mut data) = jm.get() {
                // it's safe to just pop the "max length" because pop is "best effort only"
                ret = data.pop_u128(128, JtagEndian::Little).unwrap();
            }
        }
        // only the very last sequence value is returned
        ret
    }

    fn burn_bank<T: JtagPhy>(&mut self, bank: usize, ones: u32, jm: &mut JtagMach, jp: &mut T) {
        if ones == 0 { // skip the bank if nothing to burn
            return;
        }
        let bank_select: u8 = (bank as u8 - 1) * 8 + 0xA1;
        let word_select: u8 = bank_select | 0b10;

        let bank_fuse: [(JtagChain, usize, u64, &str); 7] = [
            (JtagChain::IR, 6, 0b001100, "JSTART"),
            (JtagChain::IR, 6, 0b110000, "EFUSE"),
            (JtagChain::DR, 64, 0xa08a28ac00004001, "KEY_UNLOCK1"),
            (JtagChain::DR, 64, 0xa08a28ac00004001, "KEY_UNLOCK2"),
            (JtagChain::IR, 6, 0b110000, "EFUSE"),
            (JtagChain::DR, 64, 0xa08a28ac00000000 | bank_select as u64, "KEY_BANK"),
            (JtagChain::DR, 64, 0x0, "KEY_BANK_WAIT"),
        ];
        self.jtag_seq(jm, jp, &bank_fuse);
        let mut curbit = ones;
        for i in 0..32 {
            if (curbit & 0x1) == 1 {
                let bit_burn: [(JtagChain, usize, u64, &str); 3] = [
                    (JtagChain::IR, 6, 0b110000, "EFUSE"),
                    (JtagChain::DR, 64, (0xa08a28ac00004000 | word_select as u64) + (i as u64) << 8, "KEY_BIT"),
                    (JtagChain::DR, 64, 0x0, "KEY_BIT_WAIT"),
                ];
                self.jtag_seq(jm, jp, &bit_burn);
                curbit >>= 1;
            }
        }
        self.jtag_seq(jm, jp, &bank_fuse);
    }

    // burns fuses to the FPGA bank
    pub fn burn<T: JtagPhy>(&mut self, jm: &mut JtagMach, jp: &mut T) -> bool {
        const COMMIT_SEQ: [(JtagChain, usize, u64, &str); 22] = 
            [
                (JtagChain::DR, 64, 0xff000000ff, "EFUSE_COMMIT"),
                (JtagChain::IR, 6, 0b000010, "USER1"),
                (JtagChain::DR, 32, 0, "USER1"),
                (JtagChain::IR, 6, 0b000010, "USER1"),
                (JtagChain::DR, 17, 0xF000, "USER1"),
                (JtagChain::DR, 75, 0xA9, "USER1"),
                (JtagChain::IR, 6, 0b100010, "USER3"),
                (JtagChain::DR, 17, 0xF000, "USER3"),
                (JtagChain::DR, 75, 0xA9, "USER3"),
                (JtagChain::IR, 6, 0b111111, "BYPASS"),
                (JtagChain::IR, 6, 0b000011, "USER2"),
                (JtagChain::DR, 32, 0x0, "USER2"),
                (JtagChain::IR, 6, 0b111111, "BYPASS"),
                (JtagChain::IR, 6, 0b000011, "USER2"),
                (JtagChain::DR, 42, 0x69, "USER2"),
                (JtagChain::IR, 6, 0b111111, "BYPASS"),
                (JtagChain::IR, 6, 0b000011, "USER2"),
                (JtagChain::DR, 6, 0xC, "USER2"),
                (JtagChain::DR, 42, 0x69, "USER2"),
                (JtagChain::IR, 6, 0b111111, "BYPASS"),
                (JtagChain::IR, 6, 0b000011, "USER2"),
                (JtagChain::DR, 36, 0x0, "USER2"),
            ];

        let ok: bool = true;

        // first check if we're valid
        if !self.is_valid() {
            return false;
        }

        // reset the machine before doing any burning
        jm.reset(jp);
        
        // iterate through banks, careful to make bank 0 the last
        for index in FUSE_BANKS-1..=0 {
            if index == 0 {
                // handle cntl special case
                if ((self.phy.banks[0] & 0x3F) as u8 ^ self.cntl) != 0 {
                    // 1111_1100_0000_0011_1111
                    let new_cntl: u32 = (self.cntl as u32) | ((self.cntl as u32) << 14);
                    self.burn_bank(index, ((self.phy.banks[0] & 0xFC03F) ^ new_cntl) & new_cntl, jm, jp);
                }
            } else if index == 12 {
                // handle user special case
                if (self.phy.banks[index] ^ add_ecc(self.user >> 8)) != 0 {
                    // compute just the 0->1's and pass that on to burn_bank
                    self.burn_bank(index, self.phy.banks[index] ^ add_ecc(self.user >> 8) & add_ecc(self.user >> 8), jm, jp);
                }
            } else if index == 11 {
                // handle user + key special case
                let raw_fuse: u32 = ((self.user & 0xFF) << 16) | (self.key[31] as u32) << 8 | self.key[30] as u32;
                if (self.phy.banks[index] ^ add_ecc(raw_fuse)) != 0 {
                    self.burn_bank(index, (self.phy.banks[index] ^ add_ecc(raw_fuse)) & add_ecc(raw_fuse), jm, jp);
                }
            } else {
                // handle key fuses (most of the bank)
                let mut raw_fuse: u32 = 0;
                for i in 0..3 { 
                    raw_fuse <<= 8;
                    raw_fuse |= self.key[(index-1)*3 + 2-i] as u32; 
                }
                if (self.phy.banks[index] ^ add_ecc(raw_fuse)) != 0 {
                    self.burn_bank(index, (self.phy.banks[index] ^ add_ecc(raw_fuse)) & add_ecc(raw_fuse), jm, jp);
                }
            }

        }
        self.jtag_seq(jm, jp, &COMMIT_SEQ);
        ok
    }

}


// to see print outputs run with `cargo test -- --nocapture`
#[cfg(test)]
#[macro_use]
extern crate std;
use libc::*;

mod tests {
    use super::*;
    use jtag::*;

    #[test]
    fn it_works() {
        print!("hello world!\n");
        assert_eq!(2 + 2, 4);
    }

    #[macro_use]
    const TIMESTEP: f64 = 1e-6;
    pub struct JtagTestPhy {
        time: f64,
    }

    fn puts (s: &'_ [u8])
    {
        if s.contains(&b'\0') { unsafe {
            ::libc::puts(s.as_ptr() as *const ::libc::c_char);
        }}
    }

    impl JtagPhy for JtagTestPhy {
        fn new() -> Self {
            puts(b"time, clk, tdo, tms, tdi\0");
            JtagTestPhy {
                time: 0.0,
            }
        }

        fn sync(&mut self, tdi: bool, tms: bool) -> bool {

            let mut local_tdi: u8 = 0;
            let mut local_tms: u8 = 0;
            if tdi {
                local_tdi = 1;
            }
            if tms {
                local_tms = 1;
            }
            self.time += TIMESTEP;
            format!("hello {}", "world");
            //println!("{}, {}, {}, {}, {}", self.time, 0, 0, local_tms, local_tdi);
            self.time += TIMESTEP;
            //println!("{}, {}, {}, {}, {}", self.time, 1, 0, local_tms, local_tdi);
            self.time += TIMESTEP;
            //println!("{}, {}, {}, {}, {}", self.time, 0, 0, local_tms, local_tdi);

            false
        }

        fn nosync(&mut self, _tdi: bool, _tms: bool, _tck: bool) -> bool {
            // not actually used, not implemented -- fail if called
            assert!(false);

            false
        }
    }

    #[test]
    fn jtag_fetch() {
        let mut jm: JtagMach = JtagMach::new();
        let mut jp: JtagTestPhy = JtagTestPhy::new();

        let mut efuse: EfuseApi = EfuseApi::new();

        efuse.fetch(&mut jm, &mut jp);
    }

}
