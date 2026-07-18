cbuffer CameraCB : register(b0) {
    float4x4 viewProj;
    float aspect;       // fbo width / height
    float planeScale;   // world-extension factor (zoom boost)
    float texW;         // fbo pixel size
    float texH;
    float4 sky;         // x = night factor 0..1 (drives the sky gradient)
};
