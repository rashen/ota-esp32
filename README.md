# Over the air (OTA) updates for ESP32

Tested on esp32c3 and esp32c5.

## How-to:
1. Write an .env file in the client dir with SSID and password for your wifi
2. Build the client application image
3. Save the image using `espflash save-image`. This will later be used by the server as our new OTA image.
4. Start the client and wait for it to gain an IP address
5. Start the server with the client IP and app binary relative path as input arguments
6. Wait for OTA update to complete, reset the client
