import assert from "node:assert/strict";
import test from "node:test";

import {
  computeVideoRect,
  deviceToStagePoint,
  orientDisplayDimensions,
  stageToNormalizedDevice,
} from "../src/hud-overlay.ts";
import {
  buildAvccDescription,
  packAvccFrame,
  parseH264AnnexBPacket,
  splitAnnexBNalUnits,
} from "../src/webcodecs-player.ts";

test("splitAnnexBNalUnits handles 3-byte and 4-byte Annex-B start codes", () => {
  const buf = new Uint8Array([
    0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xe0, 0x1e, // SPS (type 7)
    0x00, 0x00, 0x01, 0x68, 0xce, 0x38, 0x80,       // PPS (type 8)
    0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84,       // IDR (type 5)
  ]);
  const nals = splitAnnexBNalUnits(buf);
  assert.equal(nals.length, 3);
  assert.equal(nals[0][0] & 0x1f, 7);
  assert.equal(nals[1][0] & 0x1f, 8);
  assert.equal(nals[2][0] & 0x1f, 5);
  assert.deepEqual(Array.from(nals[0]), [0x67, 0x42, 0xe0, 0x1e]);
});

test("buildAvccDescription produces valid avcC box and avc1 codec string", () => {
  const sps = new Uint8Array([0x67, 0x64, 0x00, 0x2a, 0xac, 0xd9]);
  const pps = new Uint8Array([0x68, 0xeb, 0xe3, 0xcb]);
  const { codec, description } = buildAvccDescription(sps, pps);

  assert.equal(codec, "avc1.64002A");
  assert.equal(description[0], 1);    // configurationVersion
  assert.equal(description[1], 0x64); // profile
  assert.equal(description[2], 0x00); // compat
  assert.equal(description[3], 0x2a); // level
  assert.equal(description[4], 0xff); // 4-byte length prefix
  assert.equal(description[5], 0xe1); // 1 SPS
  assert.equal((description[6] << 8) | description[7], sps.length);
  const ppsCountIdx = 8 + sps.length;
  assert.equal(description[ppsCountIdx], 1);
  assert.equal((description[ppsCountIdx + 1] << 8) | description[ppsCountIdx + 2], pps.length);
});

test("packAvccFrame prefixes each NAL unit with 4-byte big-endian length", () => {
  const nal1 = new Uint8Array([0x65, 0xaa, 0xbb]);
  const nal2 = new Uint8Array([0x06, 0x11]);
  const packed = packAvccFrame([nal1, nal2]);
  assert.equal(packed.length, 4 + 3 + 4 + 2);
  const view = new DataView(packed.buffer);
  assert.equal(view.getUint32(0, false), 3);
  assert.equal(packed[4], 0x65);
  assert.equal(view.getUint32(7, false), 2);
  assert.equal(packed[11], 0x06);
});

test("HUD aspect-fit coordinate mapper accurately round-trips letterboxed video space", () => {
  // Portrait 1080x2400 phone inside wide 1000x800 stage -> pillarboxed horizontally
  const vRect = computeVideoRect(1000, 800, 1080, 2400);
  assert.equal(vRect.height, 800);
  assert.equal(Math.round(vRect.width), 360);
  assert.equal(Math.round(vRect.x), 320);

  const stagePt = deviceToStagePoint(540, 1200, 1000, 800, 1080, 2400);
  assert.equal(Math.round(stagePt.x), 500);
  assert.equal(Math.round(stagePt.y), 400);

  const back = stageToNormalizedDevice(
    stagePt.x + 10,
    stagePt.y + 20,
    { left: 10, top: 20, width: 1000, height: 800 } as DOMRect,
    1080,
    2400
  );
  assert.equal(back.inside, true);
  assert.ok(Math.abs(back.normX - 0.5) < 0.001);
  assert.ok(Math.abs(back.normY - 0.5) < 0.001);

  // Outside letterbox margin -> inside = false
  const outside = stageToNormalizedDevice(
    50,
    400,
    { left: 0, top: 0, width: 1000, height: 800 } as DOMRect,
    1080,
    2400
  );
  assert.equal(outside.inside, false);
});

test("orientDisplayDimensions and parseH264AnnexBPacket handle rotation and SEI/VCL filtering", () => {
  // Portrait downscaled video preserves portrait physical display dimensions
  assert.deepEqual(orientDisplayDimensions(1080, 2424, 576, 1280), {
    width: 1080,
    height: 2424,
  });
  // Landscape rotated video flips physical display dimensions
  assert.deepEqual(orientDisplayDimensions(1080, 2424, 1280, 576), {
    width: 2424,
    height: 1080,
  });

  // SEI-only packet (NAL type 6) has hasVclSlice = false so VideoDecoder isn't fed non-VCL chunks
  const seiOnly = parseH264AnnexBPacket(new Uint8Array([0x00, 0x00, 0x00, 0x01, 0x06, 0x05, 0x01]));
  assert.equal(seiOnly.hasVclSlice, false);
  assert.equal(seiOnly.sliceNals.length, 1);

  // SPS + PPS + IDR (types 7, 8, 5) extracts parameter sets and marks hasIdr + hasVclSlice
  const idrPacket = parseH264AnnexBPacket(
    new Uint8Array([
      0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xe0, 0x1e,
      0x00, 0x00, 0x00, 0x01, 0x68, 0xce, 0x38, 0x80,
      0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84,
    ])
  );
  assert.ok(idrPacket.sps);
  assert.ok(idrPacket.pps);
  assert.equal(idrPacket.hasIdr, true);
  assert.equal(idrPacket.hasVclSlice, true);
  assert.equal(idrPacket.sliceNals.length, 1);
});
