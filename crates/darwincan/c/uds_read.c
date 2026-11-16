/* SPDX-License-Identifier: MIT OR Apache-2.0 */
/*
 * uds_read.c: read one data identifier from an ECU with UDS service 0x22
 * over ISO-TP. An example of a diagnostic request and its response.
 *
 *     cc uds_read.c -ldarwincan -o uds_read
 *     ./uds_read can0 7E0 7E8 F190      (the VIN, on most ECUs)
 */
#include <stdio.h>
#include <stdlib.h>

#include <darwincan.h>

int main(int argc, char **argv)
{
	struct dcan_isotp_options options;
	uint8_t request[3], response[4095];
	unsigned long did;
	dcan_isotp *ecu;
	int len, i;

	if (argc != 5) {
		fprintf(stderr, "usage: uds_read <interface> <tx id> <rx id> <data id>\n");
		return 2;
	}
	dcan_isotp_options_init(&options, (canid_t)strtoul(argv[2], NULL, 16),
				(canid_t)strtoul(argv[3], NULL, 16));
	/* For 29-bit identifiers, add CAN_EFF_FLAG to both. For CAN FD:
	 *     options.flags = DCAN_ISOTP_FD | DCAN_ISOTP_BRS;
	 *     options.tx_dl = 64;
	 */
	ecu = dcan_isotp_open(argv[1], &options);
	if (ecu == NULL) {
		fprintf(stderr, "uds_read: %s\n", dcan_last_error());
		return 1;
	}
	did = strtoul(argv[4], NULL, 16);
	request[0] = 0x22;
	request[1] = (uint8_t)(did >> 8);
	request[2] = (uint8_t)did;
	if (dcan_isotp_send(ecu, request, sizeof(request)) < 0) {
		fprintf(stderr, "uds_read: %s\n", dcan_last_error());
		return 1;
	}
	for (;;) {
		len = dcan_isotp_recv(ecu, response, sizeof(response), NULL, 1000);
		if (len < 0) {
			fprintf(stderr, "uds_read: %s\n", dcan_last_error());
			return 1;
		}
		if (len == 0) {
			fprintf(stderr, "uds_read: no response within a second\n");
			return 1;
		}
		/* 7F 22 78: the ECU needs more time and will answer again. */
		if (len == 3 && response[0] == 0x7F && response[2] == 0x78)
			continue;
		break;
	}
	if (len > (int)sizeof(response))
		len = sizeof(response);
	if (response[0] == 0x7F) {
		fprintf(stderr, "uds_read: negative response, code %02X\n",
			len > 2 ? response[2] : 0);
		return 1;
	}
	for (i = 0; i < len; i++)
		printf("%02X%s", response[i], i + 1 < len ? " " : "\n");
	dcan_isotp_close(ecu);
	return 0;
}
