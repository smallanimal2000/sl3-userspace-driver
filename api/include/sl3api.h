// sl3api.h — public C ABI of Sl3Api (reconstructed from Rane's shipped framework).
// The implementation is the Rust `sl3api` cdylib. This header lets new C/C++
// clients call the same ABI the original prefPane imports.
#ifndef SL3API_H
#define SL3API_H

#include <stdint.h>

typedef struct { uint8_t bytes[3]; } sl3_phonoswitch_status_data;
typedef struct { uint8_t bytes[4]; } sl3_thrustate_status_data;
typedef struct { uint8_t bytes[6]; } sl3_overload_status_data;
typedef struct { uint32_t status; uint32_t reserved; } sl3_usb_port_status_t;
typedef struct { uint32_t a, b, c, d; } sl3_version;
typedef struct { uint32_t state; uint32_t percent; } sl3_firmware_status_data;

#ifdef __cplusplus
extern "C" {
#endif

void *sl3_open(void);
int   sl3_close(void *handle);

int sl3_get_audio_controls(void *handle, int offset, void *dst, int count);
int sl3_set_audio_controls(void *handle, int offset, const void *data, int count);
int sl3_get_parameter(void *handle, int paramID, uint32_t *out);
int sl3_set_parameter(void *handle, int paramID, uint32_t value);
int sl3_get_buffer_millisecs(void *handle, uint32_t *out_ms);
int sl3_set_buffer_millisecs(void *handle, int ms);

int sl3_is_C0_device(void *handle, int *out);
int sl3_get_vendor_id(void *handle, uint32_t *out);
int sl3_get_product_id(void *handle, uint32_t *out);
int sl3_get_status(void *handle, uint8_t *out);
int sl3_get_overload_status(void *handle, sl3_overload_status_data *out);
int sl3_get_usb_port_status(void *handle, sl3_usb_port_status_t *out);
int sl3_get_driver_version(void *handle, sl3_version *out);
int sl3_get_api_version(sl3_version *out);

int sl3_set_phonoswitch_callback(void *handle, void (*cb)(sl3_phonoswitch_status_data *));
int sl3_set_thrustate_callback(void *handle, void (*cb)(sl3_thrustate_status_data *));
int sl3_set_overload_callback(void *handle, void (*cb)(sl3_overload_status_data *));
int sl3_set_usb_port_status_callback(void *handle, void (*cb)(sl3_usb_port_status_t *));

int sl3_get_firmware_version(void *handle, sl3_version *out);
int sl3_get_firmware_embedded_version(sl3_version *out);
int sl3_set_update_firmware_from_embedded(void *handle, void (*cb)(sl3_firmware_status_data *), unsigned int flags);

#ifdef __cplusplus
}
int sl3_private_last_connection_error();
#else
int sl3_private_last_connection_error(void);
#endif

#endif // SL3API_H
